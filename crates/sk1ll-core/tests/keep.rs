//! Keep mine (`Repo::pull_keeping`): pull the Baseline and put the Draft
//! back on top, through clashes, races and interruptions.
#![cfg(unix)]

mod common;

use std::fs;

use sk1ll_core::draft::{Keep, KeptPull, Unkept};

use common::*;
use sk1ll_core::draft::PullOutcome;
use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::sync::displaced;
use sk1ll_core::{EngineLock, Repo, Result};

fn keep(repo: &Repo, selection: &Keep) -> Result<KeptPull> {
    repo.pull_keeping(&EngineLock::acquire(repo.root())?, selection)
}

/// Both Machines change `pdf`; `b` also deletes a file of it and adds one.
fn kept_clash(f: &Fleet) -> (String, String) {
    let b = f.b();
    write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
    let remote = f.publish("personal/pdf/ref.md", "a's reference\n", "skill:pdf", "a");
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
    fs::remove_file(f.b.join("personal/pdf/ref.md")).unwrap();
    write(&f.b.join("personal/pdf/notes.md"), "b's notes\n");
    let base = git(&f.b, &["rev-parse", "HEAD"]);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { .. }));
    (base, remote)
}

/// Both Machines change `pdf`; `b`'s change is an unpushed commit, and
/// `b` has fetched `a`'s.
fn committed_clash(f: &Fleet) {
    let b = f.b();
    f.publish("personal/pdf/SKILL.md", "a's edit\n", "skill:pdf", "a");
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
    b.commit(&[item("skill:pdf")], "b", None).unwrap();
    b.fetch().unwrap();
}

fn assert_kept(f: &Fleet) {
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
    assert_eq!(read(&f.b.join("personal/pdf/notes.md")), "b's notes\n");
    assert!(!f.b.join("personal/pdf/ref.md").exists(), "b's deletion wins too");
    assert!(!keep_pending(&f.b()));
}

fn interrupt_keep(repo: &Repo, expected: Point, file: Option<&str>) {
    let crash = failpoints::scoped(
        |point, path| {
            if point == expected && file.is_none_or(|file| path.ends_with(file)) {
                Action::Crash
            } else {
                Action::Continue
            }
        },
        || failpoints::catch(|| keep(repo, &Keep::All)),
    )
    .expect_err("keep reached the checkpoint");
    assert_eq!(crash.point, expected);
    assert!(keep_pending(repo));
}

fn finish_keeping_now(b: &Repo) -> Option<Vec<Unkept>> {
    b.finish_keeping(&EngineLock::acquire(b.root()).unwrap()).unwrap()
}

#[test]
fn keep_mine_takes_the_baseline_and_puts_the_draft_back_on_top() {
    let f = Fleet::new();
    let b = f.b();
    let (base, remote) = kept_clash(&f);
    let kept = keep(&b, &Keep::All).unwrap();
    assert_eq!(
        kept,
        KeptPull { outcome: PullOutcome::Updated { from: base, to: remote.clone() }, kept: vec![item("skill:pdf")] }
    );
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote);
    assert_kept(&f);
    // A Draft change on the new Baseline, ready to commit.
    assert!(items_of(&b).contains(&item("skill:pdf")));
    let diff = b.diff(&item("skill:pdf")).unwrap();
    assert!(diff.contains("-a's edit") && diff.contains("+b's edit"), "{diff}");
    b.commit(&[item("skill:pdf")], "b wins", None).unwrap();
    b.push().unwrap();
}

#[test]
fn keep_mine_leaves_a_pull_blocked_on_an_item_it_doesnt_name() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("instructions/git.md"), "a's rule\n");
    a.commit(&[item("rule:git")], "a rule", None).unwrap();
    let (base, _) = kept_clash(&f);
    write(&f.b.join("instructions/git.md"), "b's rule\n");
    let draft = b.draft().unwrap();
    let kept = keep(&b, &Keep::Items(vec![item("skill:pdf")])).unwrap();
    assert!(matches!(&kept.outcome, PullOutcome::Blocked { items, .. } if items.len() == 2), "{kept:?}");
    assert!(kept.kept.is_empty());
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
    assert_eq!(b.draft().unwrap(), draft, "nothing moved");
    let err = keep(&b, &Keep::Items(vec![item("rule:style")])).unwrap_err();
    assert!(err.to_string().contains("`rule:style` is neither in the draft nor in an unpushed commit"), "{err}");
    keep(&b, &Keep::All).unwrap();
    assert_kept(&f);
    assert_eq!(read(&f.b.join("instructions/git.md")), "b's rule\n");
}

/// A kept file that can't go back (something new is at its path) stays
/// in recovery, and the error says how to finish.
#[test]
fn keep_mine_that_cant_put_a_file_back_leaves_it_in_recovery() {
    let f = Fleet::new();
    let b = f.b();
    let (_, remote) = kept_clash(&f);
    let skill = f.b.join("personal/pdf/SKILL.md");
    let newer = skill.clone();
    let _edit = failpoints::once(Point::KeepSettle, move |_| write(&newer, "written meanwhile\n"));
    let err = keep(&b, &Keep::All).unwrap_err().to_string();
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote, "the pull went through");
    assert_eq!(read(&skill), "written meanwhile\n");
    let id = err
        .split("kept in recovery as ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("{err}"));
    assert!(err.contains(&format!("`sk1ll recover adopt {id}`")), "{err}");
    let entry = displaced::get(id).unwrap();
    assert_eq!(read(&entry.path), "b's edit\n");
    assert!(!keep_pending(&b));
    // The rest went back.
    assert_eq!(read(&f.b.join("personal/pdf/notes.md")), "b's notes\n");
    b.restore(&[item("skill:pdf")]).unwrap();
    let lock = EngineLock::acquire(b.root()).unwrap();
    displaced::adopt(&b, id, &lock).unwrap();
    assert_eq!(read(&skill), "b's edit\n");
}

/// A save at a kept path after its file moved into recovery is never
/// overwritten or deleted by the restore: it stays, and the version
/// before it is in recovery, with the command to bring it back.
#[test]
fn keep_mine_never_overwrites_a_save_made_during_its_restore() {
    let f = Fleet::new();
    let b = f.b();
    let (base, _) = kept_clash(&f);
    let (skill, notes) = (f.b.join("personal/pdf/SKILL.md"), f.b.join("personal/pdf/notes.md"));
    let err = failpoints::scoped(
        |point, path| {
            if point == Point::KeepMoved {
                assert!(!path.exists(), "moved into recovery");
                if path.ends_with("SKILL.md") {
                    write(&skill, "saved again\n");
                } else if path.ends_with("notes.md") {
                    write(&notes, "notes saved again\n");
                }
            }
            Action::Continue
        },
        || keep(&b, &Keep::All),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(read(&skill), "saved again\n");
    assert_eq!(read(&notes), "notes saved again\n", "a Draft-only path isn't deleted");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base, "nothing pulled");
    assert!(err.contains("`sk1ll recover adopt "), "{err}");
    let kept: Vec<String> = displaced::list()
        .unwrap()
        .into_iter()
        .filter(|e| e.dest.starts_with(f.b.canonicalize().unwrap()) && e.changed)
        .map(|e| read(&e.path))
        .collect();
    assert!(kept.contains(&"b's edit\n".to_string()) && kept.contains(&"b's notes\n".to_string()), "{kept:?}");
}

/// Items named before the first pull may clash only through an unpushed
/// commit the pull turns back into Draft; a name that can't clash at
/// all is refused before anything changes.
#[test]
fn keep_mine_takes_names_that_clash_only_in_an_unpushed_commit() {
    let f = Fleet::new();
    let (a, b) = (f.a(), f.b());
    write(&f.a.join("personal/pdf/SKILL.md"), "a's edit\n");
    write(&f.a.join("instructions/git.md"), "a's rule\n");
    a.commit(&[item("skill:pdf"), item("rule:git")], "a", None).unwrap();
    a.push().unwrap();
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
    b.commit(&[item("skill:pdf")], "b", None).unwrap();
    write(&f.b.join("instructions/git.md"), "b's rule\n");
    b.fetch().unwrap();
    let head = git(&f.b, &["rev-parse", "HEAD"]);
    let bogus = Keep::Items(vec![item("rule:git"), item("rule:style")]);
    let err = keep(&b, &bogus).unwrap_err().to_string();
    assert!(err.contains("`rule:style` is neither in the draft nor in an unpushed commit"), "{err}");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), head, "nothing changed");
    let both = Keep::Items(vec![item("rule:git"), item("skill:pdf")]);
    let kept = keep(&b, &both).unwrap();
    assert_eq!(kept.kept, [item("skill:pdf"), item("rule:git")]);
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
    assert!(read(&f.b.join("instructions/git.md")).ends_with("b's rule\n"));
}

/// A directory made at a kept path while it is restored stops the
/// restore safely: the directory stays, the kept version stays in
/// recovery with the command to bring it back, and nothing is left that
/// stops the next pull.
#[test]
fn a_directory_in_the_way_of_a_restore_is_left_alone() {
    let f = Fleet::new();
    let b = f.b();
    let (base, _) = kept_clash(&f);
    let skill = f.b.join("personal/pdf/SKILL.md");
    let err = failpoints::scoped(
        |point, path| {
            if point == Point::KeepMoved && path == skill {
                fs::create_dir(&skill).unwrap();
                write(&skill.join("inside.md"), "a directory's file\n");
            }
            Action::Continue
        },
        || keep(&b, &Keep::All),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("`sk1ll recover adopt "), "{err}");
    assert_eq!(read(&skill.join("inside.md")), "a directory's file\n");
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), base);
    assert!(!keep_pending(&b));
    assert!(!f.b.join(".git/sk1ll-keep-stage").exists());
    // Stopped once files moved: none is put back, each is reported.
    assert!(!f.b.join("personal/pdf/notes.md").exists());
    assert!(err.contains("An interrupted keep left your versions in recovery"), "{err}");
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { .. }));
}

/// The pull's un-commit stages a committed clash: that staging is the
/// keep's own once journaled. Interrupted after that and before the keep
/// reset it, settling resets it while it is still exactly that; staged
/// since by the user, it is left as it is and reported. Interrupted
/// before it was journaled (after the file moved), its ownership is
/// uncertain: left as it is, and reported.
#[test]
fn an_interrupted_keep_resets_only_its_own_staging() {
    for (user_stages, journaled) in [(false, true), (true, true), (false, false)] {
        let f = Fleet::new();
        let b = f.b();
        committed_clash(&f);
        match journaled {
            true => interrupt_keep(&b, Point::KeepOwned, Some("SKILL.md")),
            false => interrupt_keep(&b, Point::KeepMoved, Some("SKILL.md")),
        }
        let staged = || git(&f.b, &["diff", "--cached", "--name-only"]);
        assert!(staged().contains("personal/pdf/SKILL.md"), "the un-commit's, not reset yet");
        if user_stages {
            write(&f.b.join("personal/pdf/SKILL.md"), "staged by the user\n");
            git(&f.b, &["add", "personal/pdf/SKILL.md"]);
        }
        let report = finish_keeping_now(&b).unwrap();
        let skill_staged = || staged().lines().any(|l| l == "personal/pdf/SKILL.md");
        match (user_stages, journaled) {
            (false, true) => assert!(!skill_staged(), "reset: it was the keep's own"),
            (true, _) => {
                assert!(skill_staged(), "the user's staging stays");
                assert!(report.iter().any(|u| u.error.contains("was staged since the keep stopped")), "{report:?}");
            }
            (false, false) => {
                assert!(skill_staged(), "not journaled as its own: left as it is");
                assert!(report.iter().any(|u| u.error.contains("didn't record as its own")), "{report:?}");
            }
        }
        assert!(report.iter().any(|u| u.entry.is_some()), "{report:?}");
    }
}

/// A plain pull that turned a committed clash back into Draft staged it:
/// a later keep takes that staging as the operation's own; once the
/// user stages something else there, it is theirs, and refused.
#[test]
fn keep_mine_tells_a_pull_s_staging_from_the_user_s() {
    let f = Fleet::new();
    let b = f.b();
    committed_clash(&f);
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { uncommitted, .. } if uncommitted.len() == 1));
    assert!(git(&f.b, &["diff", "--cached", "--name-only"]).contains("personal/pdf/SKILL.md"), "staged by the pull");
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit, staged again\n");
    git(&f.b, &["add", "personal/pdf/SKILL.md"]);
    let err = keep(&b, &Keep::All).unwrap_err().to_string();
    assert!(err.contains("has a staged change"), "{err}");
    git(&f.b, &["restore", "--staged", "--", "personal/pdf/SKILL.md"]);
    write(&f.b.join("personal/pdf/SKILL.md"), "b's edit\n");
    // Committed and pulled once more: the pull's own staging again.
    b.commit(&[item("skill:pdf")], "b again", None).unwrap();
    assert!(matches!(pull(&b).unwrap(), PullOutcome::Blocked { .. }));
    let kept = keep(&b, &Keep::All).unwrap();
    assert_eq!(kept.kept, [item("skill:pdf")]);
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
}

/// A pull stopped right after its un-commit moved the branch, before it
/// recorded what it staged: a keep can't tell that staging from the
/// user's, so it refuses, and nothing is lost.
#[test]
fn an_un_commit_stopped_before_recording_its_staging_counts_as_the_user_s() {
    let f = Fleet::new();
    let b = f.b();
    committed_clash(&f);
    let crash = failpoints::catch(|| {
        let _crash =
            failpoints::install(
                |point, _| if point == Point::PullUncommitted { Action::Crash } else { Action::Continue },
            );
        pull(&b)
    })
    .unwrap_err();
    assert_eq!(crash.point, Point::PullUncommitted);
    assert!(!f.b.join(".git/sk1ll-uncommitted.json").exists(), "recorded only once the branch moved");
    let err = keep(&b, &Keep::All).unwrap_err().to_string();
    assert!(err.contains("has a staged change"), "{err}");
    assert_eq!(read(&f.b.join("personal/pdf/SKILL.md")), "b's edit\n");
    assert!(git(&f.b, &["diff", "--cached", "--name-only"]).contains("personal/pdf/SKILL.md"));
}

/// An interrupted keep's report stays until the user acts: each version
/// adopted (which brings it back) or discarded drops out, and the report
/// goes with the last.
/// A recovery entry that can't be read stays listed, to check again.
#[test]
fn an_interrupted_keep_s_report_stays_until_each_version_is_acted_on() {
    let f = Fleet::new();
    let b = f.b();
    let (_, remote) = kept_clash(&f);
    interrupt_keep(&b, Point::KeepSettle, None);
    let report = finish_keeping_now(&b).unwrap();
    assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), remote, "the pull side is done");
    let ids: Vec<String> = report.iter().filter_map(|u| u.entry.clone()).collect();
    assert_eq!(ids.len(), 2, "{report:?}");
    let saved = f.b.join(".git/sk1ll-keep-report.json");
    assert!(saved.exists());
    assert_eq!(b.keep_report().unwrap(), report, "read again, still there");
    use std::os::unix::fs::PermissionsExt;
    let id = &ids[0];
    let listed = || b.keep_report().unwrap().iter().any(|u| u.entry.as_ref() == Some(id));
    let entry = displaced::get(id).unwrap();
    let dir = entry.path.parent().unwrap();
    let index = displaced::root().unwrap().join(".index").join(id);
    for file in [dir.join("entry.json"), index.clone()] {
        let bytes = fs::read(&file).unwrap();
        write(&file, "invalid json");
        assert!(listed(), "{file:?}");
        fs::write(file, bytes).unwrap();
    }
    for path in [&index, dir] {
        let permissions = fs::metadata(path).unwrap().permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        let retained = listed();
        fs::set_permissions(path, permissions).unwrap();
        assert!(retained, "{path:?}");
    }
    // Each kept version comes back with `recover adopt`.
    let lock = EngineLock::acquire(b.root()).unwrap();
    let kept = read(&entry.path);
    displaced::adopt(&b, &ids[0], &lock).unwrap();
    assert_eq!(read(&entry.dest), kept);
    drop(lock);
    let left = b.keep_report().unwrap();
    assert!(
        left.iter().all(|u| u.entry.as_ref() != Some(&ids[0]))
            && left.iter().any(|u| u.entry.as_ref() == Some(&ids[1]))
    );
    displaced::discard(&ids[1]).unwrap();
    for _ in 0..2 {
        let notes = b.keep_report().unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].entry.is_none() && notes[0].error.contains("ref.md"));
    }
    assert_eq!(b.dismiss_keep_notes().unwrap(), 1);
    assert!(b.keep_report().unwrap().is_empty());
    assert_eq!(b.dismiss_keep_notes().unwrap(), 0);
    assert!(!saved.exists());
}

/// Staging the user did at a kept path is refused before anything
/// changes, whatever it holds: staged as the file, staged differently,
/// or a staged removal with the file made again.
#[test]
fn keep_mine_refuses_a_kept_path_the_user_staged() {
    for case in ["same", "different", "removed"] {
        let f = Fleet::new();
        let b = f.b();
        kept_clash(&f);
        let rel = "personal/pdf/SKILL.md";
        match case {
            "same" => drop(git(&f.b, &["add", rel])),
            "different" => {
                git(&f.b, &["add", rel]);
                write(&f.b.join(rel), "working copy\n");
            }
            _ => drop(git(&f.b, &["rm", "--cached", "--quiet", rel])),
        }
        let before = (git(&f.b, &["diff", "--cached", "--name-only"]), read(&f.b.join(rel)));
        let head = git(&f.b, &["rev-parse", "HEAD"]);
        let err = keep(&b, &Keep::All).unwrap_err().to_string();
        assert!(err.contains("has a staged change") && err.contains("restore --staged"), "{case}: {err}");
        assert_eq!((git(&f.b, &["diff", "--cached", "--name-only"]), read(&f.b.join(rel))), before, "{case}");
        assert_eq!(git(&f.b, &["rev-parse", "HEAD"]), head, "{case}");
        assert!(!keep_pending(&b), "{case}");
        git(&f.b, &["restore", "--staged", "--", rel]);
        keep(&b, &Keep::All).unwrap();
        assert_eq!(git(&f.b, &["diff", "--cached", "--name-only"]), "", "{case}");
    }
}

#[test]
fn an_interrupted_keep_preserves_originals_and_later_edits_at_every_checkpoint() {
    let f = Fleet::new();
    let (base, upstream) = kept_clash(&f);
    let rel = "personal/pdf/SKILL.md";
    let notes = "personal/pdf/notes.md";
    write(&f.b.join("unrelated"), "staged content\n");
    git(&f.b, &["add", "unrelated"]);
    write(&f.b.join("unrelated"), "unstaged content\n");

    let probe = f.copy_b();
    let hits = {
        let record = failpoints::record();
        assert_eq!(keep(&probe.repo(), &Keep::All).unwrap().kept, [item("skill:pdf")]);
        record.hits()
    };
    for expected in [Point::KeepRestore, Point::KeepRecorded, Point::KeepMoved, Point::KeepSettle] {
        assert!(hits.iter().any(|hit| hit.point == expected), "{expected:?}");
    }
    for (n, hit) in hits.iter().enumerate() {
        let copy = f.copy_b();
        let root = copy.b.as_path();
        let case = format!("checkpoint {n} ({:?})", hit.point);
        let crash = {
            let _crash = failpoints::crash_at(n);
            failpoints::catch(|| keep(&copy.repo(), &Keep::All)).unwrap_err()
        };
        assert_eq!(crash.point, hit.point, "{case}");

        let repo = copy.repo();
        if repo.pull_pending().unwrap() {
            assert!(matches!(finish(&repo).unwrap(), Some(PullOutcome::Updated { .. })), "{case}");
        }
        let local = root.join(rel);
        let added = root.join(notes);
        let originals = [(&local, b"b's edit\n".as_slice()), (&added, b"b's notes\n".as_slice())];
        let moved = originals.map(|(path, bytes)| !fs::read(path).is_ok_and(|actual| actual == bytes));
        let before = snapshot(root);

        // Edit a moved path before settling. If its original is still live,
        // compare its bytes after settling instead of replacing them ourselves.
        if moved[0] {
            write(&local, "later edit\n");
        }
        if moved[1] && added.exists() {
            fs::remove_file(&added).unwrap();
        }
        let report = finish_keeping_now(&repo).unwrap_or_default();
        assert!(!keep_pending(&repo) && !repo.pull_pending().unwrap(), "{case}");
        assert!(!root.join(".git/sk1ll-keep-stage").exists(), "{case}");
        assert_eq!(head_ref(root), "refs/heads/main", "{case}");
        let after = snapshot(root);
        assert_eq!((after.head, after.index, after.stash), (before.head, before.index, before.stash), "{case}");
        assert_eq!(git(root, &["diff", "--cached", "--name-only"]), "unrelated", "{case}");
        let head = head(root);
        assert!(head == base || head == upstream, "{case}");
        if moved[0] {
            assert_eq!(read(&local), "later edit\n", "{case}");
        }
        if moved[1] {
            assert!(!added.exists(), "{case}");
        }
        for ((path, bytes), moved) in originals.into_iter().zip(moved) {
            if moved {
                assert!(
                    report.iter().filter_map(|u| u.entry.as_ref()).any(|id| {
                        let entry = displaced::get(id).unwrap();
                        entry.changed && entry.dest == *path && fs::read(entry.path).unwrap() == bytes
                    }),
                    "{case}: {path:?} missing from {report:?}"
                );
            } else {
                assert_eq!(fs::read(path).unwrap(), bytes, "{case}");
            }
        }
        assert_eq!(read(&root.join("unrelated")), "unstaged content\n", "{case}");
        assert_eq!(git(root, &["show", ":unrelated"]), "staged content", "{case}");
        assert_eq!(repo.keep_report().unwrap(), report, "{case}");
        assert_eq!(finish_keeping_now(&copy.repo()), None, "{case}");
    }
}
