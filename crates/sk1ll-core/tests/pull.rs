//! Pull (`Repo::pull`, `finish_pull`): fast-forward and replay of
//! unpushed commits, clashes, refusals, and a journal that lets an
//! interrupted pull finish without losing a Draft change.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use sk1ll_core::draft::{Change, PullOutcome};
use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::git::Git;
use sk1ll_core::{Error, Repo, Result};
use walkdir::WalkDir;

fn during_pull(repo: &Repo, checkpoint: (Point, usize), run: impl FnOnce()) -> Result<PullOutcome> {
    let (point, nth) = checkpoint;
    failpoints::scoped(
        at_hit(point, nth, || {
            run();
            Action::Continue
        }),
        || pull(repo),
    )
}

fn interrupt_pull(repo: &Repo, point: Point, nth: usize) {
    let crash = failpoints::scoped(at_hit(point, nth, || Action::Crash), || failpoints::catch(|| pull(repo)))
        .expect_err("pull reached the checkpoint");
    assert_eq!(crash.point, point);
}

fn at_hit(point: Point, mut nth: usize, run: impl FnOnce() -> Action) -> impl FnMut(Point, &Path) -> Action {
    let mut run = Some(run);
    move |current, _| {
        if current == point && run.is_some() {
            if nth == 0 {
                return run.take().unwrap()();
            }
            nth -= 1;
        }
        Action::Continue
    }
}

/// A caller can fetch and push without its engine lock: the local half
/// of a pull only moves onto what was fetched, and a push sends the exact
/// commit that was checked.
#[test]
fn fetching_and_pushing_split_from_the_local_steps() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let old = git(&f.b, &["rev-parse", "HEAD"]);
    write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
    let checked = a.commit(&[item("skill:pdf")], "v2", None).unwrap();
    assert_eq!(a.prepare_push().unwrap().as_deref(), Some(checked.as_str()));
    // A commit made after the check is not pushed with it.
    write(&f.a.join("instructions/git.md"), "later\n");
    let later = a.commit(&[item("rule:git")], "later", None).unwrap();
    a.push_commit(&checked).unwrap();
    assert_eq!(git(&f.a, &["rev-parse", "refs/remotes/origin/main"]), checked);
    assert_eq!(git(&f.a, &["rev-parse", "HEAD"]), later);

    let lock = sk1ll_core::EngineLock::acquire(&f.b).unwrap();
    assert_eq!(b.pull_fetched(&lock).unwrap(), PullOutcome::UpToDate, "nothing fetched yet");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old);
    b.fetch().unwrap();
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old, "a fetch moves no branch");
    assert_eq!(b.pull_fetched(&lock).unwrap(), PullOutcome::Updated { from: old, to: checked });
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
}

/// An import being staged stays where it is, even with a `.gitignore`
/// that negates sk1ll's exclude.
#[test]
fn pull_leaves_imports_being_staged_alone() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join(".git/info/exclude"), ".sk1ll-stage-*\n");
    write(&f.b.join(".gitignore"), "!.sk1ll-stage-*/\n");
    let staged = f.b.join("imported/.sk1ll-stage-1-2-3/tree/SKILL.md");
    write(&staged, "being staged\n");
    // An unpushed commit, so the pull rebases.
    write(&f.b.join("instructions/style.md"), "b committed\n");
    b.commit(&[item("rule:style")], "style from b", None).unwrap();
    write(&f.b.join("instructions/git.md"), "b draft\n");
    let draft = b.draft().unwrap();
    assert!(draft.iter().all(|d| !d.item.to_string().contains(".sk1ll-stage-")), "{draft:?}");
    // Replacing the file would give it a new mtime.
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    fs::File::options().write(true).open(&staged).unwrap().set_modified(old).unwrap();
    let mtime = || fs::metadata(&staged).unwrap().modified().unwrap();

    f.publish("personal/pdf/SKILL.md", "v2\n", "skill:pdf", "v2");
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(read(&staged), "being staged\n");
    assert_eq!(mtime(), old, "never swept away and put back");
    assert_eq!(b.draft().unwrap(), draft);
    assert_eq!(git(&f.b, &["stash", "list"]), "");
}

#[test]
fn pull_replay_detects_duplicate_ids_from_two_clones_before_push() {
    let f = Fleet::new();
    let a = f.a();
    let b = f.b();
    // Both machines committed a copy of the same skill at disjoint paths.
    a.create_item("skill", "a-copy", "").unwrap();
    let from = a.personal_dir().join("a-copy");
    let to = b.personal_dir().join("b-copy");
    fs::create_dir(&to).unwrap();
    for name in ["SKILL.md", ".sk1ll-id"] {
        fs::copy(from.join(name), to.join(name)).unwrap();
    }
    a.commit(&[item("skill:a-copy")], "Add copy on A", None).unwrap();
    b.commit(&[item("skill:b-copy")], "Add copy on B", None).unwrap();
    a.push().unwrap();
    let err = pull(&b).unwrap_err().to_string();
    assert!(err.contains("sk1ll repair-ids") && err.contains("a-copy") && err.contains("b-copy"), "{err}");
    assert!(b.push().unwrap_err().to_string().contains("sk1ll repair-ids"));
    // The copy, and the fixture's items that never had an Item ID.
    assert!(b.repair_ids().unwrap() > 1);
    assert!(b.draft().unwrap().iter().any(|change| change.item == item("skill:b-copy")));
    b.commit(&[item("skill:b-copy")], "Repair duplicate ID", None).unwrap();
    b.push().unwrap();
}

#[test]
fn pull_blocked_by_clash_changes_nothing() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join("personal/pdf/ref.md"), "b's edit\n");
    write(&f.b.join("personal/new/SKILL.md"), "b new\n");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("SKILL.md", f.b.join("personal/new/link.md")).unwrap();
        chmod(&f.b.join("instructions/style.md"), 0o755);
    }
    let remote = f.publish("personal/pdf/SKILL.md", "a's edit\n", "skill:pdf", "a");

    let before = snapshot(&f.b);
    let outcome = pull(&b).unwrap();
    assert_eq!(
        outcome,
        PullOutcome::Blocked { baseline: remote.clone(), items: vec![item("skill:pdf")], uncommitted: Vec::new() }
    );
    assert_eq!(snapshot(&f.b), before);

    // An unpushed commit that conflicts with the new baseline backs out too.
    b.restore(&[item("skill:pdf")]).unwrap();
    write(&f.b.join("personal/pdf/SKILL.md"), "b's committed edit\n");
    b.commit(&[item("skill:pdf")], "b", None).unwrap();
    write(&f.b.join("personal/pdf/ref.md"), "unrelated?\n");
    let before = snapshot(&f.b);
    // The draft touches pdf too, so this is caught before anything moves.
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { .. }));
    assert_eq!(snapshot(&f.b), before);

    // Without the draft on top, the conflicting commit turns back into
    // Draft: only the branch moves, every file stays as it was.
    b.restore(&[item("skill:pdf")]).unwrap();
    let files = snapshot(&f.b).files;
    let outcome = pull(&b).unwrap();
    let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
    assert_eq!((baseline, items), (remote, vec![item("skill:pdf")]));
    assert!(uncommitted.len() == 1 && uncommitted[0].ends_with(" b"), "{uncommitted:?}");
    assert_eq!(snapshot(&f.b).files, files);
    assert!(f.b.join("personal/new/SKILL.md").is_file());
    assert!(items_of(&b).contains(&item("skill:pdf")));
}

#[test]
fn pull_never_overwrites_ignored_files() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.b.join(".git/info/exclude"), "secret.txt\nbuild\n");
    write(&f.b.join("secret.txt"), "mine\n");
    write(&f.b.join("build"), "an ignored file where upstream wants a directory\n");
    assert!(b.draft().unwrap().is_empty());
    write(&f.a.join("secret.txt"), "upstream\n");
    write(&f.a.join("build/out.txt"), "upstream\n");
    let remote = a.commit(&[item("other:secret.txt"), item("other:build/out.txt")], "secret", None).unwrap();
    a.push().unwrap();

    // Fast-forward path.
    let before = snapshot(&f.b);
    let blocked = PullOutcome::Blocked {
        baseline: remote.clone(),
        items: vec![item("other:build/out.txt"), item("other:secret.txt")],
        uncommitted: Vec::new(),
    };
    assert_eq!(pull(&b).unwrap(), blocked);
    assert_eq!(snapshot(&f.b), before);

    // Rebase path.
    write(&f.b.join("instructions/style.md"), "b committed\n");
    b.commit(&[item("rule:style")], "style", None).unwrap();
    let before = snapshot(&f.b);
    assert_eq!(pull(&b).unwrap(), blocked);
    assert_eq!(snapshot(&f.b), before);
    assert_eq!(read(&f.b.join("secret.txt")), "mine\n");
}

/// An ignored file that appears in the way during the pull is caught by
/// pull's last checks, or else when the new file goes in (which never
/// replaces anything), on the fast-forward path and when rebasing.
#[test]
fn pull_protects_an_ignored_file_created_during_the_pull() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join(".git/info/exclude"), "private.txt\n");
    let remote = f.publish("private.txt", "upstream\n", "other:private.txt", "private");
    let path = f.b.join("private.txt");
    let blocked =
        PullOutcome::Blocked { baseline: remote, items: vec![item("other:private.txt")], uncommitted: Vec::new() };
    for rebase in [false, true] {
        if rebase {
            write(&f.b.join("instructions/style.md"), "b committed\n");
            b.commit(&[item("rule:style")], "style", None).unwrap();
        }
        for point in [(Point::PullChecks, 0), (Point::PullJournal, 0), (Point::PullFile, 0)] {
            let before = snapshot(&f.b);
            assert_eq!(during_pull(&b, point, || write(&path, "mine\n")).unwrap(), blocked, "{point:?}");
            assert_eq!(read(&path), "mine\n");
            assert!(!pending(&b), "nothing was changed, so nothing is left to finish");
            fs::remove_file(&path).unwrap();
            assert_eq!(snapshot(&f.b), before);
        }
    }
}

/// A save that lands while pull works, in an item the new baseline
/// changes, blocks the pull like any draft edit there.
#[test]
fn pull_blocks_a_save_made_during_the_pull() {
    let f = Fleet::new();
    let b = f.b();
    let remote = f.publish("personal/pdf/SKILL.md", "a's edit\n", "skill:pdf", "pdf");
    let (skill, sibling) = (f.b.join("personal/pdf/SKILL.md"), f.b.join("personal/pdf/ref.md"));
    let blocked = PullOutcome::Blocked { baseline: remote, items: vec![item("skill:pdf")], uncommitted: Vec::new() };
    for rebase in [false, true] {
        if rebase {
            write(&f.b.join("instructions/style.md"), "b committed\n");
            b.commit(&[item("rule:style")], "style", None).unwrap();
        }
        let before = snapshot(&f.b);
        assert_eq!(
            during_pull(&b, (Point::PullChecks, 0), || write(&sibling, "saved during pull\n")).unwrap(),
            blocked
        );
        assert_eq!(read(&sibling), "saved during pull\n");
        b.restore(&[item("skill:pdf")]).unwrap();
        assert_eq!(snapshot(&f.b), before);
    }
    // Each path is checked again right before it is updated. The first
    // one is the Item ID a's commit gave pdf (it sorts before SKILL.md).
    let id = f.b.join("personal/pdf/.sk1ll-id");
    assert_eq!(during_pull(&b, (Point::PullFile, 0), || write(&id, "saved\n")).unwrap(), blocked);
    assert_eq!(read(&id), "saved\n");
    assert_eq!(read(&skill), "---\nname: pdf\n---\nPDFs.\n");
    assert!(!pending(&b));
}

#[test]
fn pull_rebase_conflict_leaves_head_index_and_files_unchanged() {
    let f = Fleet::new();
    let b = f.b();
    // The first commit replays cleanly. The second conflicts.
    write(&f.b.join("instructions/style.md"), "b committed\n");
    b.commit(&[item("rule:style")], "style", None).unwrap();
    write(&f.b.join("imported/web/SKILL.md"), "b committed\n");
    let head = b.commit(&[item("skill:web")], "web from b", None).unwrap();
    let remote = f.publish("imported/web/SKILL.md", "web v2\n", "skill:web", "web v2");
    write(&f.b.join("instructions/git.md"), "staged\n");
    git(&f.b, &["add", "instructions/git.md"]);
    write(&f.b.join("instructions/git.md"), "unstaged\n");
    write(&f.b.join("personal/new/SKILL.md"), "untracked\n");
    // Force stale stat information to catch a status call writing the index.
    fs::File::options()
        .write(true)
        .open(f.b.join("personal/pdf/SKILL.md"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000))
        .unwrap();

    let files = snapshot(&f.b).files;
    let index = fs::read(f.b.join(".git/index")).unwrap();
    let style = git(&f.b, &["rev-parse", &format!("{head}^")]);
    let outcome =
        during_pull(&b, (Point::PullChecks, 0), || panic!("conflicting replay reached the tree switch")).unwrap();
    let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
    assert_eq!((baseline, items), (remote, vec![item("skill:web")]));
    assert!(uncommitted.len() == 1 && uncommitted[0].ends_with(" web from b"), "{uncommitted:?}");
    // The branch is back on the commit that replays cleanly; the
    // conflicting one is Draft again, staged, and no byte of the
    // working tree or the index changed.
    assert_eq!(snapshot(&f.b).files, files);
    assert_eq!(git(&f.b, &["rev-parse", "refs/heads/main"]), style);
    assert_eq!(fs::read(f.b.join(".git/index")).unwrap(), index);
    assert!(items_of(&b).contains(&item("skill:web")));
    assert_eq!(fs::read_to_string(f.b.join("imported/web/SKILL.md")).unwrap(), "b committed\n");
}

/// The whole way out of a clash with a committed item: the commit turns
/// back into Draft, a restore that keeps it in recovery clears the way,
/// and adopting the entry brings it back on the new Baseline.
#[test]
fn a_conflicting_unpushed_commit_turns_back_into_draft_and_comes_back_after_the_pull() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
    let remote = f.publish("personal/pdf/SKILL.md", "a's edit\n", "skill:pdf", "a");
    let base = git(&f.b, &["rev-parse", "HEAD"]);
    b.commit(&[item("skill:pdf")], "b", None).unwrap();

    let outcome = pull(&b).unwrap();
    assert!(matches!(&outcome, PullOutcome::Blocked { uncommitted, .. } if uncommitted.len() == 1), "{outcome:?}");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");

    let kept = b.restore_keeping(&[item("skill:pdf")], &[item("skill:pdf")].into()).unwrap();
    let entries: Vec<_> = kept.iter().map(|id| sk1ll_core::sync::displaced::get(id).unwrap()).collect();
    let skill = entries.iter().find(|e| e.dest.ends_with("personal/pdf/SKILL.md")).expect("SKILL.md is kept");
    assert_eq!(pull(&b).unwrap(), PullOutcome::Updated { from: base, to: remote });
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "a's edit\n");
    let _lock = sk1ll_core::EngineLock::acquire(b.root()).unwrap();
    sk1ll_core::sync::displaced::adopt(&b, &skill.id, &_lock).unwrap();
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
    assert!(items_of(&b).contains(&item("skill:pdf")));
}

/// A repo started on its own (not cloned) on `c`'s branch, with the fleet's origin.
fn independent(f: &Fleet, branch: &str, origin: &Path) -> PathBuf {
    let c = f.tmp.path().join(format!("c-{branch}"));
    git(f.tmp.path(), &["init", "--quiet", "-b", branch, c.to_str().unwrap()]);
    git(&c, &["config", "user.name", "Machine C"]);
    git(&c, &["config", "user.email", "c@example.com"]);
    write(&c.join("sk1ll.toml"), "# c\n");
    commit_all(&c, "c root");
    git(&c, &["remote", "add", "origin", origin.to_str().unwrap()]);
    c
}

fn origin_branches(origin: &Path) -> String {
    git(origin, &["for-each-ref", "--format=%(refname)", "refs/heads/"])
}

/// Upstream changes for the tests of pulls that stop half-way: in pull's
/// order, a removal, then a modification, an addition and an
/// executable change in three items. `b` gets a draft pull carries:
/// a staged new file, a staged and then edited rule, an untracked
/// skill. Returns the new baseline.
fn half_way_setup(f: &Fleet) -> String {
    fs::remove_dir_all(f.a.join("imported/web")).unwrap();
    write(&f.a.join("instructions/git.md"), "git v2\n");
    write(&f.a.join("personal/pdf/SKILL.md"), "---\nname: pdf\n---\nPDFs v2.\n");
    write(&f.a.join("personal/pdf/extra.md"), "added\n");
    #[cfg(unix)]
    chmod(&f.a.join("personal/pdf/ref.md"), 0o755);
    commit_all(&f.a, "upstream");
    git(&f.a, &["push", "--quiet", "origin", "main"]);
    write(&f.b.join("notes.txt"), "staged\n");
    write(&f.b.join("instructions/style.md"), "staged style\n");
    git(&f.b, &["add", "notes.txt", "instructions/style.md"]);
    write(&f.b.join("instructions/style.md"), "unstaged style\n");
    write(&f.b.join("personal/mine/SKILL.md"), "untracked\n");
    git(&f.a, &["rev-parse", "HEAD"])
}

fn assert_at_upstream(root: &Path) {
    assert!(!root.join("imported/web").exists());
    assert_eq!(read(&root.join("instructions/git.md")), "git v2\n");
    assert_eq!(read(&root.join("personal/pdf/SKILL.md")), "---\nname: pdf\n---\nPDFs v2.\n");
    assert_eq!(read(&root.join("personal/pdf/extra.md")), "added\n");
    assert!(!cfg!(unix) || executable(&root.join("personal/pdf/ref.md")));
    assert_eq!(git(root, &["show", ":notes.txt"]), "staged");
    assert_eq!(git(root, &["show", ":instructions/style.md"]), "staged style");
    assert_eq!(read(&root.join("instructions/style.md")), "unstaged style\n");
    assert_eq!(read(&root.join("personal/mine/SKILL.md")), "untracked\n");
    assert_eq!(head_ref(root), "refs/heads/main");
    let anchor = Git::new(root).args(["rev-parse", "--verify", "--quiet", "refs/worktree/sk1ll-pull"]);
    assert!(!anchor.output().unwrap().status.success(), "the target's anchor is gone with the journal");
    assert_eq!(sk1ll_core::sync::displaced::changed().unwrap(), []);
}

/// Configures the filter driver `name` in `b`, whose smudge upper-cases
/// `v2` and whose clean lower-cases it again, each noting in the
/// returned log file that it ran.
#[cfg(unix)]
fn logged_driver(b: &Path, name: &str) -> PathBuf {
    let log = b.join(".git/driver-ran");
    git(b, &["config", &format!("filter.{name}.clean"), &format!("echo >>{}; sed s/V2/v2/", log.display())]);
    git(b, &["config", &format!("filter.{name}.smudge"), &format!("echo >>{}; sed s/v2/V2/", log.display())]);
    git(b, &["config", &format!("filter.{name}.required"), "true"]);
    log
}

#[test]
fn pull_rebases_without_touching_the_draft_or_author_metadata() {
    let f = Fleet::new();
    let b = f.b();
    {
        let _date = failpoints::git_env(&[("GIT_AUTHOR_DATE", "1700000000 +0230")]);
        write(&f.b.join("instructions/style.md"), "b committed\n");
        b.commit(&[item("rule:style")], "Style\n\nKeep this body.\n", Some("Éditor <editor@example.com>")).unwrap();
        write(&f.b.join("personal/local/SKILL.md"), "b committed too\n");
        b.commit(&[item("skill:local")], "Local skill", Some("Other Author <other@example.com>")).unwrap();
    }
    let authors_and_messages =
        |range: &str| Git::new(&f.b).args(["log", "--format=%an <%ae> %ai%n%B", "--reverse", range]).run().unwrap();
    let original = authors_and_messages("origin/main..HEAD");
    let head = git(&f.b, &["rev-parse", "HEAD"]);
    let remote = f.publish("imported/web/SKILL.md", "web v2\n", "skill:web", "web v2");

    let tracked = f.b.join("instructions/git.md");
    let untracked = f.b.join("personal/new/SKILL.md");
    write(&tracked, "staged\n");
    git(&f.b, &["add", "instructions/git.md"]);
    write(&tracked, "unstaged\n");
    write(&untracked, "untracked\n");
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    for path in [&tracked, &untracked, &f.b.join("imported/web/SKILL.md")] {
        fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
    }
    let check_draft = || {
        assert_eq!(fs::read(&tracked).unwrap(), b"unstaged\n");
        assert_eq!(fs::read(&untracked).unwrap(), b"untracked\n");
        for path in [&tracked, &untracked] {
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), old);
        }
        assert_eq!(git(&f.b, &["show", ":instructions/git.md"]), "staged");
        assert_eq!(git(&f.b, &["stash", "list"]), "");
    };
    let late = f.b.join("personal/new/late.md");
    check_draft();
    let outcome = during_pull(&b, (Point::PullChecks, 0), || {
        check_draft();
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), head);
        write(&late, "saved during pull\n");
    })
    .unwrap();
    assert!(matches!(outcome, PullOutcome::Updated { from, to } if from == head && to != head));
    check_draft();
    assert_eq!(fs::read(&late).unwrap(), b"saved during pull\n");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD~2"]), remote);
    assert_eq!(authors_and_messages("origin/main..HEAD"), original);
    assert_eq!(git(&f.b, &["log", "-1", "--format=%cn", "HEAD"]), hostname());
    assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
}

#[test]
fn pull_leaves_an_identical_stash_in_place() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    // a: an unpushed commit, so the pull rebases; b moves the baseline.
    write(&f.a.join("instructions/style.md"), "a committed\n");
    a.commit(&[item("rule:style")], "style", None).unwrap();
    write(&f.b.join("imported/web/SKILL.md"), "web v2\n");
    b.commit(&[item("skill:web")], "web v2", None).unwrap();
    b.push().unwrap();
    write(&f.a.join("instructions/git.md"), "a draft\n");
    write(&f.a.join("notes.txt"), "untracked\n");
    git(&f.a, &["stash", "push", "--include-untracked", "--quiet", "-m", "sk1ll pull: draft"]);
    git(&f.a, &["stash", "apply", "--index", "--quiet"]);

    let stashes = || git(&f.a, &["stash", "list", "--format=%H %gs"]);
    let before = stashes();
    assert!(matches!(pull(&a).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(stashes(), before);
    assert_eq!(read(&f.a.join("instructions/git.md")), "a draft\n");
    assert_eq!(read(&f.a.join("notes.txt")), "untracked\n");
}

#[test]
fn pull_drops_commits_that_become_empty() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    for root in [&f.a, &f.b] {
        write(&root.join("personal/pdf/SKILL.md"), "v2\n");
        // The same commit on both: with the same Item ID too.
        write(&root.join("personal/pdf/.sk1ll-id"), "pdf-id\n");
        Repo::open(root).unwrap().commit(&[item("skill:pdf")], "pdf v2", None).unwrap();
    }
    git(
        &f.b,
        &[
            "-c",
            "user.name=Machine B",
            "-c",
            "user.email=b@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "originally empty",
        ],
    );
    a.push().unwrap();

    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(git(&f.b, &["log", "--format=%s", "origin/main..HEAD"]), "originally empty");
    assert!(b.draft().unwrap().is_empty());
}

#[test]
fn pull_refuses_unpushed_merge_commits() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    git(&f.a, &["checkout", "--quiet", "-b", "topic"]);
    write(&f.a.join("instructions/style.md"), "topic\n");
    commit_all(&f.a, "topic");
    git(&f.a, &["checkout", "--quiet", "main"]);
    git(&f.a, &["merge", "--quiet", "--no-ff", "topic"]);
    write(&f.b.join("imported/web/SKILL.md"), "web v2\n");
    b.commit(&[item("skill:web")], "web v2", None).unwrap();
    b.push().unwrap();

    let before = snapshot(&f.a);
    assert!(invalid(pull(&a)).contains("merge commits among the unpushed commits"));
    assert_eq!(snapshot(&f.a), before);
}

#[test]
fn pull_refuses_unrelated_histories_without_touching_anything() {
    // Two machines joined an empty server and each made its own root commit.
    let f = Fleet::new();
    let c = independent(&f, "main", &f.origin);
    write(&c.join("instructions/git.md"), "c draft\n");
    let r = Repo::open(&c).unwrap();
    let before = snapshot(&c);
    for _ in 0..2 {
        let message = invalid(pull(&r));
        assert!(message.contains("unrelated histories"), "{message}");
        assert_eq!(snapshot(&c), before);
    }
    assert_eq!(items(&r), [("rule:git".to_string(), Change::Added)]);
}

#[test]
fn pull_and_push_refuse_a_branch_other_than_the_fleet_branch() {
    // Adopted from an independent `git init` on `master`: there is no
    // `origin/master`, which must not read as "up to date".
    let f = Fleet::new();
    let origin = f.origin.clone();
    let c = independent(&f, "master", &origin);
    let r = Repo::open(&c).unwrap();
    write(&c.join("personal/pdf/SKILL.md"), "c's pdf\n");
    let before = snapshot(&c);
    for message in [invalid(pull(&r)), invalid(r.push()), invalid(r.commit(&[item("skill:pdf")], "m", None))] {
        assert!(message.contains("`master`") && message.contains("`main`"), "{message}");
        assert!(message.contains("branch -m master main"), "{message}");
    }
    assert_eq!(snapshot(&c), before);
    assert_eq!(origin_branches(&origin), "refs/heads/main");
}

#[test]
fn pull_needs_the_fleet_branch_on_origin_unless_origin_is_empty() {
    let f = Fleet::new();
    // An origin whose only branch isn't `main`.
    let other = f.tmp.path().join("other.git");
    git(f.tmp.path(), &["init", "--quiet", "--bare", "-b", "main", other.to_str().unwrap()]);
    git(&f.a, &["push", "--quiet", other.to_str().unwrap(), "main:refs/heads/trunk"]);
    let c = independent(&f, "main", &other);
    let r = Repo::open(&c).unwrap();
    let before = snapshot(&c);
    let message = invalid(pull(&r));
    assert!(message.contains("no `main` branch") && message.contains("trunk"), "{message}");
    assert_eq!(snapshot(&c), before);

    // An empty origin (an older server): nothing to pull, and the first push creates `main`.
    let empty = f.tmp.path().join("empty.git");
    git(f.tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
    git(&c, &["remote", "set-url", "origin", empty.to_str().unwrap()]);
    assert_eq!(pull(&r).unwrap(), PullOutcome::UpToDate);
    r.push().unwrap();
    assert_eq!(origin_branches(&empty), "refs/heads/main");
    assert_eq!(pull(&r).unwrap(), PullOutcome::UpToDate);
}

/// Config that would make git stash the draft, squash or run hooks
/// doesn't reach pull's switch.
#[test]
fn pull_ignores_autostash_merge_options_and_hooks() {
    let f = Fleet::new();
    let b = f.b();
    git(&f.b, &["config", "merge.autoStash", "true"]);
    git(&f.b, &["config", "rebase.autoStash", "true"]);
    git(&f.b, &["config", "branch.main.mergeOptions", "--squash"]);
    write(&f.b.join("notes.txt"), "staged new\n");
    write(&f.b.join("instructions/git.md"), "staged\n");
    git(&f.b, &["add", "notes.txt", "instructions/git.md"]);
    write(&f.b.join("instructions/git.md"), "unstaged\n");
    let draft = b.draft().unwrap();
    #[cfg(unix)]
    for hook in ["post-checkout", "post-merge", "reference-transaction"] {
        let path = f.b.join(".git/hooks").join(hook);
        write(&path, "#!/bin/sh\ntouch hook-ran\nexit 1\n");
        chmod(&path, 0o755);
    }
    let remote = f.publish("personal/pdf/SKILL.md", "v2\n", "skill:pdf", "v2");
    let old = git(&f.b, &["rev-parse", "HEAD"]);

    let staged_new = || assert_eq!(git(&f.b, &["show", ":notes.txt"]), "staged new");
    let outcome = during_pull(&b, (Point::PullUpdateRef, 0), staged_new).unwrap();
    assert_eq!(outcome, PullOutcome::Updated { from: old, to: remote.clone() });
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote);
    assert_eq!(git(&f.b, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
    assert_eq!(b.draft().unwrap(), draft);
    assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "instructions/git.md\nnotes.txt");
    assert_eq!(git(&f.b, &["show", ":instructions/git.md"]), "staged");
    assert_eq!(read(&f.b.join("instructions/git.md")), "unstaged\n");
    assert_eq!(git(&f.b, &["stash", "list"]), "");
    assert!(!f.b.join("hook-ran").exists());
}

#[test]
fn pull_never_writes_into_or_replaces_a_staging_directory() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.b.join(".git/info/exclude"), ".sk1ll-stage-*\n");
    let backup = f.b.join("imported/.sk1ll-stage-old-web/SKILL.md");
    write(&backup, "the previous web\n");
    let inside = "imported/.sk1ll-stage-old-web/extra.md";
    write(&f.a.join(inside), "upstream\n");
    git(&f.a, &["add", "--force", inside]);
    git(&f.a, &["commit", "--quiet", "-m", "into a staging directory"]);
    a.push().unwrap();
    let before = snapshot(&f.b);
    let outcome = pull(&b).unwrap();
    assert!(matches!(&outcome, PullOutcome::Blocked { items, .. } if *items == [item("skill:.sk1ll-stage-old-web")]));
    assert_eq!(snapshot(&f.b), before);

    // A file where a directory with an (empty) staging directory is: git
    // would remove the staging directory to make room.
    fs::remove_file(&backup).unwrap();
    fs::create_dir(f.b.join("imported/.sk1ll-stage-1-2-3")).unwrap();
    git(&f.a, &["rm", "-r", "--quiet", "imported"]);
    write(&f.a.join("imported"), "now a file\n");
    commit_all(&f.a, "imported is a file");
    a.push().unwrap();
    let outcome = pull(&b).unwrap();
    assert!(
        matches!(&outcome, PullOutcome::Blocked { items, .. } if items.contains(&item("other:imported"))),
        "{outcome:?}"
    );
    assert!(f.b.join("imported/.sk1ll-stage-1-2-3").is_dir());
    assert!(f.b.join("imported/.sk1ll-stage-old-web").is_dir());
}

/// The skills repo can be a subdirectory of a larger worktree. Checkout
/// updates all of it, so pull checks all of it.
#[test]
fn pull_checks_the_whole_worktree_around_a_skills_subdirectory() {
    let tmp = Scratch::new();
    let path = |p: &str| tmp.join(p);
    let origin = path("origin.git");
    let (a, b) = (path("a"), path("b"));
    git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
    git(tmp.path(), &["init", "--quiet", "-b", "main", a.to_str().unwrap()]);
    git(&a, &["config", "user.name", "Machine A"]);
    git(&a, &["config", "user.email", "a@example.com"]);
    write(&a.join("skills/sk1ll.toml"), "");
    write(&a.join("skills/personal/pdf/SKILL.md"), "---\nname: pdf\n---\nPDFs.\n");
    write(&a.join("README.md"), "the larger project\n");
    commit_all(&a, "seed");
    git(&a, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&a, &["push", "--quiet", "-u", "origin", "main"]);
    git(tmp.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    let r = Repo::open(b.join("skills")).unwrap();

    // An ignored file next to the skills repo, where upstream adds one.
    write(&a.join("notes.txt"), "upstream\n");
    commit_all(&a, "notes");
    git(&a, &["push", "--quiet", "origin", "main"]);
    let remote = git(&a, &["rev-parse", "HEAD"]);
    write(&b.join(".git/info/exclude"), "/notes.txt\n");
    write(&b.join("notes.txt"), "mine\n");
    let before = snapshot(&b);
    let blocked = |items: &[&str]| PullOutcome::Blocked {
        baseline: git(&a, &["rev-parse", "HEAD"]),
        items: items.iter().map(|i| item(i)).collect(),
        uncommitted: Vec::new(),
    };
    assert_eq!(pull(&r).unwrap(), blocked(&["other:../notes.txt"]));
    // Also with an unpushed commit, so that pull rebases.
    write(&b.join("skills/personal/pdf/SKILL.md"), "b committed\n");
    r.commit(&[item("skill:pdf")], "pdf from b", Some("B <b@example.com>")).unwrap();
    let before_rebase = snapshot(&b);
    assert_eq!(pull(&r).unwrap(), blocked(&["other:../notes.txt"]));
    assert_eq!(snapshot(&b), before_rebase);
    assert_eq!(read(&b.join("notes.txt")), "mine\n");
    assert_ne!(before, before_rebase);

    // A local change outside the skills repo to a file upstream changes.
    fs::remove_file(b.join("notes.txt")).unwrap();
    write(&a.join("README.md"), "upstream README\n");
    commit_all(&a, "readme");
    git(&a, &["push", "--quiet", "origin", "main"]);
    write(&b.join("README.md"), "b's README\n");
    assert!(r.draft().unwrap().is_empty());
    let before = snapshot(&b);
    assert_eq!(pull(&r).unwrap(), blocked(&["other:../README.md"]));
    assert_eq!(snapshot(&b), before);

    git(&b, &["checkout", "--quiet", "--", "README.md"]);
    assert!(matches!(pull(&r).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(read(&b.join("notes.txt")), "upstream\n");
    assert_eq!(read(&b.join("README.md")), "upstream README\n");
    assert_eq!(read(&b.join("skills/personal/pdf/SKILL.md")), "b committed\n");
    assert_eq!(git(&b, &["rev-parse", "HEAD~1"]), git(&a, &["rev-parse", "HEAD"]));
    assert_ne!(remote, git(&a, &["rev-parse", "HEAD"]));
}

#[test]
fn pull_refuses_submodules() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let seed = git(&f.a, &["rev-parse", "HEAD"]);
    let gitlink = format!("160000,{seed},vendor/lib");
    let as_b = ["-c", "user.name=Machine B", "-c", "user.email=b@example.com"];
    // Only in the index, with nothing to pull, and with something.
    git(&f.b, &["update-index", "--add", "--cacheinfo", &gitlink]);
    let before = snapshot(&f.b);
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);
    f.publish("instructions/git.md", "git v2\n", "rule:git", "git v2");
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);
    git(&f.b, &["update-index", "--force-remove", "vendor/lib"]);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    // In HEAD.
    git(&f.b, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(&f.b, &[&as_b[..], &["commit", "--quiet", "-m", "submodule"]].concat());
    let before = snapshot(&f.b);
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);
    f.publish("personal/pdf/SKILL.md", "v2\n", "skill:pdf", "v2");
    let before = snapshot(&f.b);
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);

    // In the new baseline, even with config that hides it or recurses into it.
    git(&f.b, &["reset", "--quiet", "--hard", "HEAD~1"]);
    git(&f.b, &["config", "submodule.recurse", "true"]);
    git(&f.b, &["config", "diff.ignoreSubmodules", "all"]);
    git(&f.a, &["update-index", "--add", "--cacheinfo", &gitlink]);
    git(&f.a, &["commit", "--quiet", "-m", "submodule"]);
    a.push().unwrap();
    let before = snapshot(&f.b);
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);

    // Adding and removing a gitlink in the same range is unsupported too.
    git(&f.a, &["rm", "--cached", "--quiet", "vendor/lib"]);
    git(&f.a, &["commit", "--quiet", "-m", "no submodule"]);
    a.push().unwrap();
    assert!(invalid(pull(&b)).contains("submodules aren't supported"));
    assert_eq!(snapshot(&f.b), before);
}

#[test]
fn pull_refuses_sparse_checkouts() {
    let f = Fleet::new();
    let b = f.b();
    f.publish("personal/pdf/SKILL.md", "v2\n", "skill:pdf", "v2");
    let before = snapshot(&f.b);
    git(&f.b, &["config", "core.sparseCheckout", "true"]);
    assert!(invalid(pull(&b)).contains("sparse checkouts aren't supported"));
    assert_eq!(snapshot(&f.b), before);
    git(&f.b, &["config", "--unset", "core.sparseCheckout"]);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
}

/// The conflict is named even when the unpushed commits end up where
/// they started (an edit, then its revert), and both turn back into Draft.
#[test]
fn pull_names_the_items_a_replayed_commit_conflicts_in() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join("instructions/style.md"), "b's style\n");
    b.commit(&[item("rule:style")], "style", None).unwrap();
    write(&f.b.join("instructions/style.md"), "Be terse.\n");
    b.commit(&[item("rule:style")], "revert style", None).unwrap();
    let remote = f.publish("instructions/style.md", "a's style\n", "rule:style", "style");
    // Both commits turn back into Draft: the first conflicts, and the
    // second is built on it. What they leave (the rule's new Item ID)
    // is in the way as Draft.
    let base = git(&f.b, &["rev-parse", "HEAD~2"]);
    let files = snapshot(&f.b).files;
    let outcome = pull(&b).unwrap();
    let PullOutcome::Blocked { baseline, items, uncommitted } = outcome else { panic!("{outcome:?}") };
    assert_eq!((baseline, items), (remote, vec![item("rule:style")]));
    assert_eq!(uncommitted.len(), 2, "{uncommitted:?}");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
    assert_eq!(snapshot(&f.b).files, files);
}

/// A replayed commit is the original object with a new tree, parent and
/// committer, minus its signature.
#[test]
fn pull_replays_commits_byte_for_byte() {
    let f = Fleet::new();
    let b = f.b();
    write(&f.b.join("instructions/style.md"), "b committed\n");
    git(&f.b, &["add", "instructions/style.md"]);
    let (tree, parent) = (git(&f.b, &["write-tree"]), git(&f.b, &["rev-parse", "HEAD"]));
    let author = b"author Caf\xe9 Author <c@example.com> 1700000000 +0230\n".as_slice();
    let kept = [author, b"encoding ISO-8859-1\nx-extra kept\n\nCaf\xe9\n\nBody \xe9.\n"].concat();
    let original = [
        format!("tree {tree}\nparent {parent}\n").as_bytes(),
        author,
        b"committer Old <old@example.com> 1700000001 +0100\nencoding ISO-8859-1\n",
        b"gpgsig -----BEGIN PGP SIGNATURE-----\n \n c2lnbmF0dXJl\n -----END PGP SIGNATURE-----\n",
        b"gpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n c2lnbmF0dXJl\n -----END PGP SIGNATURE-----\n",
        b"x-extra kept\n\nCaf\xe9\n\nBody \xe9.\n",
    ]
    .concat();
    let commit = Git::new(&f.b).args(["hash-object", "-t", "commit", "-w", "--stdin"]).stdin(original);
    git(&f.b, &["update-ref", "refs/heads/main", &commit.text().unwrap()]);
    let remote = f.publish("imported/web/SKILL.md", "web v2\n", "skill:web", "web v2");

    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    let replayed = Git::new(&f.b).args(["cat-file", "commit", "HEAD"]).run().unwrap();
    let (header, rest) = replayed.split_at(replayed.windows(2).position(|w| w == b"\n\n").unwrap() + 1);
    let lines: Vec<&[u8]> = header.split_inclusive(|b| *b == b'\n').collect();
    assert_eq!(lines[0], format!("tree {}\n", git(&f.b, &["rev-parse", "HEAD^{tree}"])).as_bytes());
    assert_eq!(lines[1], format!("parent {remote}\n").as_bytes());
    assert!(lines[3].starts_with(format!("committer {} <", hostname()).as_bytes()));
    assert_eq!([lines[2], &lines[4..].concat(), rest].concat(), kept);
    assert_eq!(git(&f.b, &["show", "HEAD:instructions/style.md"]), "b committed");
}

/// A save that lands while pull replaces a file is never lost: the swap
/// stands and the save is kept in the recovery area, from where it can
/// be adopted back as a draft change.
#[cfg(unix)]
#[test]
fn a_save_during_a_file_swap_is_kept_in_recovery() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let path = f.b.join("personal/pdf/SKILL.md");
    type Save<'a> = (&'a str, Box<dyn Fn() + 'a>);
    let mut saves: Vec<Save> = vec![(
        "saved in place\n",
        Box::new(|| {
            // Through the file being replaced, as most editors save.
            let mut file = fs::OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
            std::io::Write::write_all(&mut file, b"saved in place\n").unwrap();
        }),
    )];
    // A new file renamed into place: only an exchange brings it back.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    saves.push((
        "saved by rename\n",
        Box::new(|| {
            let tmp = f.b.join("personal/pdf/.SKILL.md.swp");
            fs::write(&tmp, "saved by rename\n").unwrap();
            fs::rename(&tmp, &path).unwrap();
        }),
    ));
    for (n, (saved, save)) in saves.into_iter().enumerate() {
        let upstream = format!("v{n}\n");
        write(&f.a.join("personal/pdf/SKILL.md"), &upstream);
        let remote = a.commit(&[item("skill:pdf")], "pdf", None).unwrap();
        a.push().unwrap();
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        match during_pull(&b, (Point::PullSwap, 0), save) {
            Err(Error::Displaced { kept, id, .. }) => {
                assert_eq!(read(&kept), saved);
                let entry = sk1ll_core::sync::displaced::get(&id).unwrap();
                assert!(entry.changed && entry.kind == sk1ll_core::sync::displaced::Kind::Draft, "{entry:?}");
                assert_eq!(entry.dest, f.b.canonicalize().unwrap().join("personal/pdf/SKILL.md"));
                // The pull is done all the same.
                assert_eq!(read(&path), upstream);
                assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote);
                assert_ne!(remote, old);
                assert!(!pending(&b) && b.draft().unwrap().is_empty());
                sk1ll_core::sync::displaced::adopt(&b, &id, &sk1ll_core::EngineLock::acquire(b.root()).unwrap())
                    .unwrap();
                assert_eq!(read(&path), saved);
                assert_eq!(items(&b), [("skill:pdf".into(), Change::Modified)]);
                b.restore(&[item("skill:pdf")]).unwrap();
            }
            other => panic!("expected the save to be displaced, got {other:?}"),
        }
    }
    assert!(
        sk1ll_core::sync::displaced::changed().unwrap().is_empty(),
        "adopted entries are gone; the content adopt replaced was the baseline's"
    );
}

/// A draft change that appears, after the last checks, in a path pull
/// has yet to update stops it there as Blocked. The pull already updated
/// other paths, so its journal stays until that item is committed or
/// restored; then the next pull finishes.
#[test]
fn a_draft_change_after_the_checks_blocks_the_rest_of_the_pull() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("imported/web/SKILL.md"), "web v2\n");
    write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
    commit_all(&f.a, "web and pdf");
    a.push().unwrap();
    let remote = git(&f.a, &["rev-parse", "HEAD"]);
    let old = git(&f.b, &["rev-parse", "HEAD"]);
    let pdf = f.b.join("personal/pdf/SKILL.md");
    let outcome = during_pull(&b, (Point::PullFile, 1), || write(&pdf, "draft edit\n")).unwrap();
    assert_eq!(
        outcome,
        PullOutcome::Blocked { baseline: remote.clone(), items: vec![item("skill:pdf")], uncommitted: Vec::new() }
    );
    assert_eq!(read(&pdf), "draft edit\n");
    assert_eq!(read(&f.b.join("imported/web/SKILL.md")), "web v2\n");
    assert!(pending(&b));
    assert_eq!((head_ref(&f.b), git(&f.b, &["rev-parse", "HEAD"])), ("refs/heads/main".into(), old.clone()));
    // Still Blocked, and nothing else can move the branch meanwhile.
    assert_eq!(finish(&b).unwrap().unwrap(), outcome);
    assert!(matches!(b.commit(&[item("skill:pdf")], "m", None), Err(Error::PullPending)));
    // Nor does anything write Item IDs into its partial tree, or push.
    assert!(matches!(b.push(), Err(Error::PullPending)));
    assert!(matches!(b.repair_ids(), Err(Error::PullPending)));
    assert!(matches!(b.assign_missing_ids(), Err(Error::PullPending)));

    b.restore(&[item("skill:pdf")]).unwrap();
    assert_eq!(pull(&b).unwrap(), PullOutcome::Updated { from: old, to: remote });
    assert_eq!(read(&pdf), "pdf v2\n");
    assert!(!pending(&b) && b.draft().unwrap().is_empty());
}

/// Item-level protection ends at the last checks: a save after them to
/// a file of a changed item that the new baseline leaves alone is no
/// clash, and stays as a draft change on top of it.
#[test]
fn a_save_to_an_untouched_file_of_a_changed_item_stays_in_the_draft() {
    let f = Fleet::new();
    let b = f.b();
    let remote = f.publish("personal/pdf/SKILL.md", "pdf v2\n", "skill:pdf", "pdf");
    let sibling = f.b.join("personal/pdf/ref.md");
    let outcome = during_pull(&b, (Point::PullFile, 0), || write(&sibling, "saved after the checks\n")).unwrap();
    assert!(matches!(outcome, PullOutcome::Updated { to, .. } if to == remote));
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "pdf v2\n");
    assert_eq!(read(&sibling), "saved after the checks\n");
    assert_eq!(items(&b), [("skill:pdf".into(), Change::Modified)]);
}

/// Another Git process holding the index lock is an error, with git's
/// message, never Blocked. Before the journal, nothing changes; after
/// the files, the next pull finishes.
#[test]
fn an_index_lock_is_an_error_not_a_clash() {
    let f = Fleet::new();
    let b = f.b();
    let remote = half_way_setup(&f);
    let old = git(&f.b, &["rev-parse", "HEAD"]);
    let lock = f.b.join(".git/index.lock");
    write(&lock, "");
    let before = snapshot(&f.b);
    match pull(&b) {
        Err(Error::Git { stderr, .. }) => assert!(stderr.contains("index.lock"), "{stderr}"),
        other => panic!("expected git's error, got {other:?}"),
    }
    assert_eq!(snapshot(&f.b), before);
    assert!(!pending(&b));
    fs::remove_file(&lock).unwrap();

    match during_pull(&b, (Point::PullIndex, 0), || write(&lock, "")) {
        Err(Error::Git { args, stderr }) => {
            assert!(args.contains("read-tree") && stderr.contains("index.lock"), "{args}: {stderr}")
        }
        other => panic!("expected git's error, got {other:?}"),
    }
    assert!(pending(&b));
    fs::remove_file(&lock).unwrap();
    assert_eq!(pull(&b).unwrap(), PullOutcome::Updated { from: old, to: remote });
    assert_at_upstream(&f.b);
}

/// Files are compared and written as git converts them, so a pull that
/// stopped after writing a converted file tells it from a draft change.
#[test]
fn a_pull_resumes_over_converted_line_endings() {
    // Conversion from config.
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    git(&f.b, &["config", "core.autocrlf", "true"]);
    write(&f.a.join("instructions/git.md"), "git v2\n");
    write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
    commit_all(&f.a, "two files");
    a.push().unwrap();
    interrupt_pull(&b, Point::PullFile, 1);
    assert_eq!(fs::read(f.b.join("instructions/git.md")).unwrap(), b"git v2\r\n");
    assert!(matches!(pull(&f.b()).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
    assert_eq!(git(&f.b, &["status", "--porcelain"]), "");

    // Conversion from `.gitattributes` the pull itself adds: each side
    // is read with its own commit's, whatever the working tree has yet.
    let f = Fleet::new();
    let b = f.b();
    write(&f.a.join(".gitattributes"), "*.md text eol=crlf\n");
    write(&f.a.join("instructions/git.md"), "git v2\n");
    write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
    commit_all(&f.a, "crlf");
    git(&f.a, &["push", "--quiet", "origin", "main"]);
    interrupt_pull(&b, Point::PullFile, 2);
    assert_eq!(fs::read(f.b.join("instructions/git.md")).unwrap(), b"git v2\r\n");
    assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"---\nname: pdf\n---\nPDFs.\n");
    assert!(matches!(pull(&f.b()).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
    assert_eq!(git(&f.b, &["status", "--porcelain"]), "");
}

/// A Baseline that writes a file through a filter driver (Git LFS, say:
/// the server has no LFS endpoint) is refused before the pull changes
/// anything, wherever the attribute comes from and whether or not this
/// Machine has the driver, and no driver runs. `git check-attr` prints
/// `set` both for `filter` without a value (no driver) and for a driver
/// named `set`: one configured under that name counts.
#[cfg(unix)]
#[test]
fn a_pull_through_a_filter_driver_is_refused_and_runs_no_driver() {
    for (attributes, line, driver, refused) in [
        (".gitattributes", "*.md filter=up", Some("up"), true),
        (".git/info/attributes", "*.md filter=lfs", None, true),
        (".gitattributes", "*.md filter=set", Some("set"), true),
        (".gitattributes", "*.md filter", None, false),
    ] {
        let case = format!("{attributes}: {line}");
        let f = Fleet::new();
        let (a, b) = (f.a(), f.b());
        let log = driver.map(|name| logged_driver(&f.b, name));
        let local = attributes.starts_with(".git/");
        write(&if local { &f.b } else { &f.a }.join(attributes), &format!("{line}\n"));
        write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
        commit_all(&f.a, "v2");
        a.push().unwrap();
        let before = head(&f.b);
        let pulled = pull(&b);
        assert!(log.as_ref().is_none_or(|log| !log.exists()), "{case}: a filter driver ran");
        if !refused {
            assert!(matches!(pulled.unwrap(), PullOutcome::Updated { .. }), "{case}");
            assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "pdf v2\n");
            continue;
        }
        let message = pulled.as_ref().map(|_| String::new()).unwrap_or_else(|e| e.to_string());
        match pulled {
            Err(Error::FilterDriver { paths, items, .. }) => {
                assert_eq!(paths, [PathBuf::from("personal/pdf/SKILL.md")], "{case}");
                assert!(items.contains(&item("skill:pdf")), "{case}: {items:?}");
            }
            other => panic!("{case}: not refused: {other:?}"),
        }
        if !local {
            assert!(message.contains("remove `filter=` from .gitattributes"), "{case}: {message}");
        }
        assert!(!pending(&b), "{case}: not started");
        assert_eq!(head(&f.b), before, "{case}");
        assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), SEED_PDF, "{case}");
        assert!(!f.b.join(".gitattributes").exists(), "{case}");
        if local {
            fs::remove_file(f.b.join(attributes)).unwrap();
            assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }), "{case}");
        }
    }
}

/// A pull that started before attributes named a driver finishes, with
/// every driver off: no driver runs, and the other conversions (line
/// endings here) still apply.
#[cfg(unix)]
#[test]
fn a_resumed_pull_finishes_with_filter_drivers_off() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("instructions/git.md"), "git v2\n");
    write(&f.a.join("personal/pdf/SKILL.md"), "pdf v2\n");
    commit_all(&f.a, "two files");
    a.push().unwrap();
    interrupt_pull(&b, Point::PullFile, 1);
    assert!(b.pull_pending().unwrap());
    let log = logged_driver(&f.b, "up");
    write(&f.b.join(".git/info/attributes"), "*.md filter=up text eol=crlf\n");
    assert!(matches!(finish(&b).unwrap(), Some(PullOutcome::Updated { .. })));
    assert!(!log.exists(), "a filter driver ran");
    assert!(!b.pull_pending().unwrap());
    assert_eq!(fs::read(f.b.join("personal/pdf/SKILL.md")).unwrap(), b"pdf v2\r\n");
}

#[cfg(unix)]
#[test]
fn pull_keeps_permissions_and_sets_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let mode = |path: &str| fs::metadata(f.b.join(path)).unwrap().permissions().mode() & 0o7777;
    write(&f.a.join("personal/pdf/run.sh"), "#!/bin/sh\n");
    chmod(&f.a.join("personal/pdf/run.sh"), 0o755);
    commit_all(&f.a, "a script");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert!(executable(&f.b.join("personal/pdf/run.sh")));

    chmod(&f.b.join("personal/pdf/SKILL.md"), 0o600);
    chmod(&f.b.join("personal/pdf/run.sh"), 0o700);
    chmod(&f.b.join("instructions/git.md"), 0o640);
    assert!(b.draft().unwrap().is_empty());
    write(&f.a.join("personal/pdf/SKILL.md"), "v2\n");
    write(&f.a.join("personal/pdf/run.sh"), "#!/bin/sh\necho v2\n");
    write(&f.a.join("instructions/git.md"), "git v2\n");
    chmod(&f.a.join("instructions/git.md"), 0o755);
    commit_all(&f.a, "changes");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "v2\n");
    assert_eq!(mode("personal/pdf/SKILL.md"), 0o600);
    assert_eq!(mode("personal/pdf/run.sh"), 0o700);
    assert_eq!(mode("instructions/git.md"), 0o750);
    assert_eq!(git(&f.b, &["status", "--porcelain"]), "");

    chmod(&f.a.join("instructions/git.md"), 0o644);
    commit_all(&f.a, "not executable");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(mode("instructions/git.md"), 0o640);
}

/// Another Git command that switches branches while pull runs stops it
/// with an error: pull changes nothing more, on either branch. Back on
/// the branch, the next pull finishes.
#[test]
fn pull_stops_when_another_command_switches_branches() {
    for (n, point) in [(Point::PullFile, 1), (Point::PullIndex, 0), (Point::PullUpdateRef, 0)].into_iter().enumerate() {
        let f = Fleet::new();
        let b = f.b();
        let remote = half_way_setup(&f);
        let old = git(&f.b, &["rev-parse", "HEAD"]);
        let staged = git(&f.b, &["diff", "--cached", "--name-only"]);
        let other = format!("other-{n}");
        let error = invalid(during_pull(&b, point, || {
            git(&f.b, &["switch", "--quiet", "-c", &other]);
        }));
        assert!(error.contains("switched") && error.contains(&format!("refs/heads/{other}")), "{error}");
        assert_eq!(head_ref(&f.b), format!("refs/heads/{other}"));
        assert_eq!(git(&f.b, &["rev-parse", "main", &other]), format!("{old}\n{old}"));
        if point == (Point::PullIndex, 0) {
            assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), staged);
        }
        if point == (Point::PullFile, 1) {
            // Only the first path was updated, before the switch.
            assert_eq!(read(&f.b.join("instructions/git.md")), "Use Conventional Commits.\n");
        }
        assert!(pending(&b));
        git(&f.b, &["switch", "--quiet", "main"]);
        assert_eq!(pull(&b).unwrap(), PullOutcome::Updated { from: old.clone(), to: remote });
        assert_at_upstream(&f.b);
        assert_eq!(git(&f.b, &["rev-parse", &other]), old);
    }
}

/// Another Git command that moves the branch while pull runs makes
/// pull stop with an error, and every later pull too: it only ever
/// moves the branch from where it was, and says what is left.
#[test]
fn pull_stops_when_another_command_moves_the_branch() {
    let f = Fleet::new();
    let b = f.b();
    f.publish("personal/pdf/SKILL.md", "v2\n", "skill:pdf", "v2");
    let head = git(&f.b, &["rev-parse", "HEAD"]);
    let tree = format!("{head}^{{tree}}");
    let moved = git(&f.b, &["-c", "user.name=T", "-c", "user.email=t@e", "commit-tree", "-p", &head, "-m", "x", &tree]);
    let result = during_pull(&b, (Point::PullUpdateRef, 0), || {
        git(&f.b, &["update-ref", "refs/heads/main", &moved]);
    });
    match result {
        Err(Error::Git { args, stderr }) => {
            assert!(args.starts_with("update-ref"), "{args}");
            assert!(stderr.contains("moved refs/heads/main"), "{stderr}");
        }
        other => panic!("expected an update-ref error, got {other:?}"),
    }
    assert_eq!(head_ref(&f.b), "refs/heads/main");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), moved);
    assert!(pending(&b));
    let before = snapshot(&f.b);
    for result in [pull(&b), finish(&b).map(|_| PullOutcome::UpToDate)] {
        let error = invalid(result);
        assert!(error.contains("another Git command moved it") && error.contains("sk1ll-pull.json"), "{error}");
        assert_eq!(snapshot(&f.b), before);
    }
}

#[cfg(unix)]
#[test]
fn a_failed_write_rolls_forward_on_the_next_pull() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root can write into the read-only directory below
    }
    let f = Fleet::new();
    let b = f.b();
    let remote = half_way_setup(&f);
    let old = git(&f.b, &["rev-parse", "HEAD"]);
    let draft = b.draft().unwrap();
    // The removal goes first, `instructions/git.md` next.
    chmod(&f.b.join("instructions"), 0o555);
    let result = pull(&b);
    chmod(&f.b.join("instructions"), 0o755);
    match result {
        Err(Error::Io { path, .. }) => assert!(path.ends_with("instructions/git.md"), "{}", path.display()),
        other => panic!("expected the write to fail, got {other:?}"),
    }
    assert!(pending(&b));
    assert!(!f.b.join("imported/web/SKILL.md").exists(), "rolled forward, never back");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), old);

    // A save to a path pull already updated is a draft change now.
    let web = f.b.join("imported/web/SKILL.md");
    write(&web, "saved meanwhile\n");
    let outcome = pull(&f.b()).unwrap();
    assert_eq!(outcome, PullOutcome::Updated { from: old, to: remote });
    assert_eq!(read(&web), "saved meanwhile\n");
    let after = b.draft().unwrap();
    assert!(after.iter().any(|d| d.item == item("skill:web")));
    for original in draft {
        assert!(after.contains(&original));
    }
}

/// Recovery entries of what pull replaced are in the git directory,
/// never in the working tree, and nothing is left staged.
#[test]
fn pull_keeps_the_working_tree_free_of_its_own_files() {
    let f = Fleet::new();
    let b = f.b();
    half_way_setup(&f);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    let names: Vec<String> = WalkDir::new(&f.b)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into())
        .collect();
    assert!(names.iter().all(|n| !n.starts_with(".sk1ll")), "{names:?}");
    let entries = sk1ll_core::sync::displaced::list().unwrap();
    assert!(
        !entries.is_empty() && entries.iter().all(|e| e.path.starts_with(f.b.join(".git").canonicalize().unwrap()))
    );
    assert!(!f.b.join(".git/sk1ll-pull-stage").exists());
    // Each holds just the committed version: a routine copy, kept for the
    // grace period and then pruned like one.
    assert!(entries.iter().all(|e| !e.changed), "{entries:?}");
    use sk1ll_core::sync::displaced;
    assert!(displaced::prune().unwrap().is_empty());
    assert_eq!(displaced::prune_with(std::time::Duration::ZERO).unwrap().len(), entries.len());
    assert_eq!(displaced::list().unwrap(), []);
}

/// What a clean pull replaced is kept as long as it may hold something Git
/// doesn't: a write through a descriptor opened before the pull, landing
/// after it, or a local extended attribute.
#[cfg(unix)]
#[test]
fn pull_entries_keep_late_writes_and_local_metadata() {
    use sk1ll_core::sync::displaced;
    use std::io::Write;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let (git_md, style_md) = ("instructions/git.md", "instructions/style.md");
    write(&f.a.join(git_md), "new rule\n");
    write(&f.a.join(style_md), "new style\n");
    commit_all(&f.a, "replace both rules");
    a.push().unwrap();
    let mut open = fs::OpenOptions::new().append(true).open(f.b.join(git_md)).unwrap();
    // Only where the file system takes user attributes.
    let xattr = rustix::fs::setxattr(f.b.join(style_md), "user.note", b"local", rustix::fs::XattrFlags::empty())
        .is_ok()
        && cfg!(target_os = "linux");
    assert!(b.draft().unwrap().is_empty());
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    open.write_all(b"saved after the pull\n").unwrap();
    drop(open);
    displaced::prune_with(std::time::Duration::ZERO).unwrap();
    let kept = displaced::list().unwrap();
    let at = |rel: &str| kept.iter().find(|e| e.dest == f.b.join(rel));
    let written = at(git_md).expect("the late write is kept");
    assert!(written.changed && read(&written.path).ends_with("saved after the pull\n"), "{written:?}");
    assert_eq!(read(&f.b.join(git_md)), "new rule\n");
    if xattr {
        let marked = at(style_md).expect("the local attribute is kept");
        assert!(marked.changed, "{marked:?}");
    }
}

#[cfg(unix)]
#[test]
fn pull_probes_exchange_before_starting() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("instructions/git.md"), "new\n");
    commit_all(&f.a, "replace a rule");
    a.push().unwrap();
    let before = snapshot(&f.b);
    let no_exchange = failpoints::fail(Point::PullExchange, std::io::ErrorKind::Unsupported);
    let result = pull(&b);
    drop(no_exchange);
    assert!(invalid(result).contains("atomic file exchange"));
    assert!(!pending(&b));
    assert_eq!(snapshot(&f.b), before);
}

/// `ident` (like an end-of-line conversion) hides local bytes from the
/// clean view Git and Pull compare by. What Pull replaces is pruned only if
/// it is exactly what checking the old commit out writes there.
#[cfg(unix)]
#[test]
fn pull_keeps_local_bytes_the_old_commit_cant_give_back() {
    use sk1ll_core::sync::displaced;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let (clean, edited) = ("instructions/clean.md", "instructions/edited.md");
    write(&f.a.join(".gitattributes"), "instructions/*.md ident\n");
    for rel in [clean, edited] {
        write(&f.a.join(rel), "$Id$\nrule\n");
    }
    commit_all(&f.a, "ident rules");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert!(read(&f.b.join(clean)).starts_with("$Id: "));
    let local = "$Id: a local note $\nrule\n";
    write(&f.b.join(edited), local);
    // Staged, it is the same blob: the tree reads as clean.
    git(&f.b, &["add", edited]);
    assert!(b.draft().unwrap().is_empty(), "the edit reads as clean");
    for rel in [clean, edited] {
        write(&f.a.join(rel), "$Id$\nnew rule\n");
    }
    commit_all(&f.a, "new rules");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    displaced::prune_with(std::time::Duration::ZERO).unwrap();
    let left = displaced::list().unwrap();
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].changed && left[0].dest == f.b.join(edited), "{left:?}");
    assert_eq!(read(&left[0].path), local);
}

/// Git records only a file's executable bit: a private mode on a file a
/// pull removes is kept, changed.
#[cfg(unix)]
#[test]
fn pull_keeps_a_mode_git_cant_give_back() {
    use sk1ll_core::sync::displaced;
    use std::os::unix::fs::PermissionsExt;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/git.md";
    fs::set_permissions(f.b.join(rel), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(b.draft().unwrap().is_empty());
    fs::remove_file(f.a.join(rel)).unwrap();
    commit_all(&f.a, "remove the rule");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    displaced::prune_with(std::time::Duration::ZERO).unwrap();
    let kept = displaced::list().unwrap();
    let entry = kept.iter().find(|e| e.dest == f.b.join(rel)).expect("the private mode is kept");
    assert!(entry.changed, "{entry:?}");
    assert_eq!(fs::metadata(&entry.path).unwrap().permissions().mode() & 0o777, 0o600);
}

/// With `core.symlinks=false` a symlink is checked out as a file holding its
/// target: a private mode on one is kept as well.
#[cfg(unix)]
#[test]
fn pull_keeps_a_mode_git_cant_give_back_on_a_symlink_placeholder() {
    use sk1ll_core::sync::displaced;
    use std::os::unix::fs::PermissionsExt;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/link";
    std::os::unix::fs::symlink("git.md", f.a.join(rel)).unwrap();
    commit_all(&f.a, "add a link");
    a.push().unwrap();
    git(&f.b, &["config", "core.symlinks", "false"]);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    let placeholder = f.b.join(rel);
    assert!(fs::symlink_metadata(&placeholder).unwrap().is_file());
    fs::set_permissions(&placeholder, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(f.a.join(rel)).unwrap();
    commit_all(&f.a, "remove the link");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    displaced::prune_with(std::time::Duration::ZERO).unwrap();
    let kept = displaced::list().unwrap();
    let entry = kept.iter().find(|e| e.dest == placeholder).expect("the private mode is kept");
    assert!(entry.changed, "{entry:?}");
}

/// The grace period runs from when the swap completed: a pull that waited
/// between reserving an entry and filling it doesn't shorten it.
#[cfg(unix)]
#[test]
fn pull_entries_get_their_grace_period_from_when_the_swap_completed() {
    use sk1ll_core::sync::displaced;
    let f = Fleet::new();
    let a = f.a();
    write(&f.a.join("instructions/git.md"), "new rule\n");
    commit_all(&f.a, "replace a rule");
    a.push().unwrap();
    interrupt_pull(&f.b(), Point::PullSwap, 0);
    let mut aged = 0;
    for dir in fs::read_dir(f.b.join(".git/.sk1ll-tmp/displaced")).unwrap() {
        let meta = dir.unwrap().path().join("entry.json");
        if let Ok(text) = fs::read_to_string(&meta) {
            let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
            json["displaced_at"] = 0.into();
            fs::write(&meta, json.to_string()).unwrap();
            aged += 1;
        }
    }
    assert_eq!(aged, 1);
    assert!(matches!(finish(&f.b()).unwrap(), Some(PullOutcome::Updated { .. })));
    assert!(displaced::prune().unwrap().is_empty());
    let entries = displaced::list().unwrap();
    assert!(entries.len() == 1 && !entries[0].changed, "{entries:?}");
    assert_eq!(displaced::prune_with(std::time::Duration::ZERO).unwrap(), [entries[0].id.clone()]);
}

#[cfg(unix)]
#[test]
fn recovery_keeps_open_writers_through_prune_and_adoption() {
    use sk1ll_core::sync::displaced::{self, Kind};
    use std::io::Write;

    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/git.md";
    write(&f.a.join(rel), "new rule\n");
    write(&f.a.join("instructions/style.md"), "new style\n");
    commit_all(&f.a, "replace both rules");
    a.push().unwrap();
    let path = f.b.join(rel);
    let mut open = fs::OpenOptions::new().append(true).open(&path).unwrap();
    assert!(matches!(
        during_pull(&b, (Point::PullFile, 1), || {
            // The first file's entry waits for the pull, however old.
            assert!(displaced::prune_with(std::time::Duration::ZERO).unwrap().is_empty());
            open.write_all(b"saved while pull was running\n").unwrap();
        })
        .unwrap(),
        PullOutcome::Updated { .. }
    ));
    let entry = displaced::changed().unwrap().into_iter().find(|e| e.kind == Kind::Draft).unwrap();
    assert_eq!(entry.dest, path);
    assert!(read(&entry.path).ends_with("saved while pull was running\n"));
    let wrong = sk1ll_core::EngineLock::acquire(&f.a).unwrap();
    assert!(invalid(displaced::adopt(&b, &entry.id, &wrong)).contains("different repository"));
    let lock = sk1ll_core::EngineLock::acquire(&f.b).unwrap();
    displaced::adopt(&b, &entry.id, &lock).unwrap();
    open.write_all(b"saved after adoption\n").unwrap();
    assert!(read(&path).ends_with("saved after adoption\n"));
    assert!(displaced::get(&entry.id).is_err());
}

#[cfg(unix)]
#[test]
fn draft_adoption_refuses_an_ignored_destination_absent_from_head() {
    use sk1ll_core::sync::displaced;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/git.md";
    fs::remove_file(f.a.join(rel)).unwrap();
    commit_all(&f.a, "remove the rule");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == f.b.join(rel)).unwrap();
    write(&f.b.join(".git/info/exclude"), &format!("/{rel}\n"));
    write(&f.b.join(rel), "private replacement\n");
    assert!(b.draft().unwrap().is_empty());
    let lock = sk1ll_core::EngineLock::acquire(&f.b).unwrap();
    let before = snapshot(&f.b);
    assert!(invalid(displaced::adopt(&b, &entry.id, &lock)).contains("local content"));
    assert_eq!(snapshot(&f.b), before);
    assert_eq!(read(&entry.path), "Use Conventional Commits.\n");
    fs::remove_file(f.b.join(rel)).unwrap();
    displaced::adopt(&b, &entry.id, &lock).unwrap();
    assert_eq!(read(&f.b.join(rel)), "Use Conventional Commits.\n");
}

#[cfg(unix)]
#[test]
fn a_directory_displaced_during_removal_is_reported_and_preserved() {
    use sk1ll_core::sync::displaced;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/git.md";
    fs::remove_file(f.a.join(rel)).unwrap();
    commit_all(&f.a, "delete rule");
    a.push().unwrap();
    let dest = f.b.join(rel);
    let result = during_pull(&b, (Point::PullRemove, 0), || {
        fs::remove_file(&dest).unwrap();
        write(&dest.join("draft"), "important directory\n");
    });
    let Err(Error::Displaced { dest: from, kept, id }) = result else {
        panic!("expected a displaced directory, got {result:?}");
    };
    assert_eq!(from, dest);
    assert_eq!(read(&kept.join("draft")), "important directory\n");
    assert!(displaced::get(&id).unwrap().changed);
    // Even an old, malformed metadata file cannot make content prunable.
    write(&kept.with_file_name("entry.json"), "{");
    assert!(!displaced::prune().unwrap().contains(&id));
    assert_eq!(read(&kept.join("draft")), "important directory\n");
}

#[cfg(unix)]
#[test]
fn resumed_pull_leaves_later_edits_to_published_paths_as_draft() {
    for operation in ["add", "replace", "remove"] {
        let f = Fleet::new();
        let a = f.a();
        let rel = if operation == "add" { "instructions/new.md" } else { "instructions/git.md" };
        if operation == "remove" {
            fs::remove_file(f.a.join(rel)).unwrap();
        } else {
            write(&f.a.join(rel), "new baseline\n");
        }
        commit_all(&f.a, operation);
        a.push().unwrap();
        interrupt_pull(&f.b(), Point::PullPublished, 0);
        let dest = f.b.join(rel);
        match operation {
            "add" => fs::remove_file(&dest).unwrap(),
            "remove" => {
                use sk1ll_core::sync::displaced;
                let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == dest).unwrap();
                assert!(invalid(displaced::discard(&entry.id)).contains("pull pending"));
                write(&dest, "recreated after publication\n");
            }
            _ => {
                let temp = dest.with_extension("save");
                write(&temp, "saved after publication\n");
                fs::rename(&temp, &dest).unwrap();
                git(&f.b, &["add", rel]);
            }
        }
        let b = f.b();
        assert!(matches!(finish(&b).unwrap(), Some(PullOutcome::Updated { .. })), "{operation}");
        assert!(!pending(&b));
        assert_eq!(b.draft().unwrap().len(), 1, "{operation}");
        match operation {
            "add" => assert!(!dest.exists()),
            "remove" => assert_eq!(read(&dest), "recreated after publication\n"),
            _ => {
                assert_eq!(read(&dest), "saved after publication\n");
                assert_eq!(git(&f.b, &["show", &format!(":{rel}")]), "saved after publication");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn pull_replaces_a_nested_directory_with_a_file() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("a/b/c"), "tracked\n");
    commit_all(&f.a, "nested directory");
    a.push().unwrap();
    pull(&b).unwrap();
    fs::create_dir_all(f.b.join("a/b/empty/deeper")).unwrap();
    fs::remove_dir_all(f.a.join("a")).unwrap();
    write(&f.a.join("a"), "now a file\n");
    commit_all(&f.a, "replace nested directory");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert_eq!(read(&f.b.join("a")), "now a file\n");
}

#[cfg(unix)]
#[test]
fn pull_recovers_byte_paths_and_full_length_leaf_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let long = "x".repeat(255);
    let names = [OsStr::from_bytes(b"invalid-\xff"), OsStr::new(&long)];
    for path in names {
        write(&f.a.join(path), "old\n");
    }
    commit_all(&f.a, "add unusual paths");
    a.push().unwrap();
    pull(&b).unwrap();
    for path in names {
        write(&f.a.join(path), "new\n");
    }
    commit_all(&f.a, "replace unusual paths");
    a.push().unwrap();
    interrupt_pull(&b, Point::PullPublished, 0);
    finish(&f.b()).unwrap();
    for path in names {
        assert_eq!(read(&f.b.join(path)), "new\n");
        let entry =
            sk1ll_core::sync::displaced::list().unwrap().into_iter().find(|e| e.dest == f.b.join(path)).unwrap();
        assert!(entry.id.len() < 255);
        assert_eq!(read(&entry.path), "old\n");
        assert!(serde_json::to_vec(&entry).is_ok());
    }
}

#[cfg(unix)]
#[test]
fn pull_applies_mode_only_changes_and_recovers_a_racing_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    let rel = "instructions/git.md";
    let path = f.b.join(rel);
    git(&f.b, &["config", "core.fileMode", "false"]);
    chmod(&f.a.join(rel), 0o755);
    commit_all(&f.a, "make rule executable");
    a.push().unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert!(executable(&path));
    chmod(&f.a.join(rel), 0o644);
    commit_all(&f.a, "make rule nonexecutable");
    a.push().unwrap();
    let result = during_pull(&b, (Point::PullSwap, 0), || chmod(&path, 0o700));
    let Err(Error::Displaced { kept, .. }) = result else { panic!("{result:?}") };
    assert_eq!(fs::metadata(&kept).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(read(&kept), read(&path));
    assert!(!executable(&path));
}

#[cfg(unix)]
#[test]
fn pull_accepts_a_case_only_rename_on_case_folding_filesystems() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("lowercase"), "baseline\n");
    commit_all(&f.a, "lowercase");
    a.push().unwrap();
    pull(&b).unwrap();
    let alias = f.b.join("Lowercase");
    let case_folding = alias.exists();
    if !case_folding {
        // On a case-sensitive filesystem a hard link with the exact
        // new spelling is a distinct untracked obstruction.
        write(&f.b.join(".git/info/exclude"), "/Lowercase\n");
        fs::hard_link(f.b.join("lowercase"), &alias).unwrap();
    }
    git(&f.a, &["mv", "-f", "lowercase", "Lowercase"]);
    git(&f.a, &["commit", "-qm", "change case"]);
    a.push().unwrap();
    assert_eq!(git(&f.a, &["ls-tree", "--name-only", "HEAD", "--", "Lowercase"]), "Lowercase");
    if !case_folding {
        assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { .. }));
        fs::remove_file(&alias).unwrap();
    }
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Updated { .. }));
    assert!(fs::read_dir(&f.b).unwrap().any(|e| e.unwrap().file_name() == "Lowercase"));
}

#[test]
fn pull_requires_the_repos_engine_lock() {
    let f = Fleet::new();
    let b = f.b();
    let wrong = sk1ll_core::EngineLock::acquire(&f.a).unwrap();
    assert!(invalid(b.pull(&wrong)).contains("different repository"));
    assert!(invalid(b.finish_pull(&wrong)).contains("different repository"));
}

#[test]
fn a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes() {
    use sk1ll_testkit::child;
    if let Some(root) = child::dir() {
        let _exit = failpoints::install(|point, _| {
            if point == Point::PullIndex { Action::Exit(child::CRASHED) } else { Action::Continue }
        });
        pull(&Repo::open(root).unwrap()).unwrap();
        panic!("pull missed the index checkpoint");
    }
    // Each takes a while: fast-forward and rebase run side by side.
    std::thread::scope(|s| {
        s.spawn(|| restart_sweep(false));
        restart_sweep(true);
    });
}

/// The body of [`a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes`].
fn restart_sweep(rebase: bool) {
    use sk1ll_testkit::child;
    let f = Fleet::new();
    let upstream = half_way_setup(&f);
    if rebase {
        write(&f.b.join("sk1ll.toml"), "# b committed\n");
        f.b().commit(&[item("manifest")], "manifest from b", None).unwrap();
    }
    let old = head(&f.b);
    let draft = f.b().draft().unwrap();
    let probe = f.copy_b();
    let hits = {
        let record = failpoints::record();
        assert!(matches!(pull(&probe.repo()).unwrap(), PullOutcome::Updated { .. }));
        record.hits()
    };
    for expected in [Point::PullJournal, Point::PullRemove, Point::PullSwap, Point::PullIndex, Point::PullDone] {
        assert!(hits.iter().any(|hit| hit.point == expected), "{expected:?}");
    }

    // From the journal on, every stop leaves it and the target's anchor,
    // and the next pull or a refresh (`finish_pull`) finishes it.
    let journal = hits.iter().position(|hit| hit.point == Point::PullJournal).unwrap();
    for (n, hit) in hits.iter().enumerate() {
        let journaled = n > journal;
        for by_pull in [true, false].into_iter().take(if journaled { 2 } else { 1 }) {
            let copy = f.copy_b();
            let root = copy.b.as_path();
            let by = if by_pull { "pull" } else { "refresh" };
            let case = format!("rebase={rebase}, hit {n} ({:?}), finished by {by}", hit.point);
            if !rebase && hit.point == Point::PullIndex {
                let name = "a_pull_restarts_at_every_checkpoint_without_losing_draft_or_staged_changes";
                let out = copy.home.apply(&mut child::command(name, root)).output().unwrap();
                assert_eq!(out.status.code(), Some(child::CRASHED), "{case}: {}", sk1ll_testkit::text(&out));
            } else {
                let crash = {
                    let _crash = failpoints::crash_at(n);
                    failpoints::catch(|| pull(&copy.repo())).unwrap_err()
                };
                assert_eq!(crash.point, hit.point, "{case}");
            }
            let repo = copy.repo();
            assert_eq!(head_ref(root), "refs/heads/main", "{case}");
            assert_eq!(repo.pull_pending().unwrap(), journaled, "{case}");
            let anchor = journaled.then(|| git(root, &["rev-parse", "refs/worktree/sk1ll-pull"]));
            let outcome = if by_pull { pull(&repo).unwrap() } else { finish(&repo).unwrap().unwrap() };
            let target = head(root);
            assert_eq!(outcome, PullOutcome::Updated { from: old.clone(), to: target.clone() }, "{case}");
            assert_eq!(anchor.unwrap_or_else(|| target.clone()), target, "{case}: the anchor kept the target");
            assert_eq!(git(root, &["rev-parse", if rebase { "HEAD~1" } else { "HEAD" }]), upstream, "{case}");
            assert!(!repo.pull_pending().unwrap(), "{case}");
            assert_eq!(repo.draft().unwrap(), draft, "{case}");
            assert_at_upstream(root);
            assert_eq!(finish(&copy.repo()).unwrap(), None, "{case}");
            assert_eq!(pull(&copy.repo()).unwrap(), PullOutcome::UpToDate, "{case}");
            // However it stopped, what it replaced ends up routine copies,
            // which pruning takes once they are old enough.
            use sk1ll_core::sync::displaced;
            assert!(displaced::list().unwrap().iter().all(|e| !e.changed), "{case}");
            displaced::prune_with(std::time::Duration::ZERO).unwrap();
            assert_eq!(displaced::list().unwrap(), [], "{case}");
        }
    }
}
