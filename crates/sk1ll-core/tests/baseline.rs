//! Joining a fleet (`sk1ll_core::git::plumbing`): adopting the first
//! Baseline into an unborn repo, cloning and adopting existing clones.
#![cfg(unix)]

mod common;

use common::{Scratch, git, read, write};
use sk1ll_core::failpoints::{self, Point};
use sk1ll_core::git::plumbing::*;
use sk1ll_core::{ItemRef, Repo, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// A bare server whose `main` holds one Baseline commit (`sk1ll.toml`).
fn server(s: &Scratch) -> String {
    let seed = s.join("server-seed");
    git(s.path(), &["init", "--quiet", "-b", "main", seed.to_str().unwrap()]);
    write(&seed.join("sk1ll.toml"), "");
    common::commit_all(&seed, "baseline");
    let bare = s.join("server.git");
    git(s.path(), &["clone", "--quiet", "--bare", seed.to_str().unwrap(), bare.to_str().unwrap()]);
    bare.to_string_lossy().into_owned()
}

/// A repo made with `git init` on `branch`, not cloned, pointed at `remote`.
fn independent(s: &Scratch, branch: &str, remote: &str) -> PathBuf {
    let dir = s.join(format!("own-{branch}"));
    git(s.path(), &["init", "--quiet", "-b", branch, dir.to_str().unwrap()]);
    write(&dir.join("sk1ll.toml"), "# mine\n");
    common::commit_all(&dir, "my own root");
    git(&dir, &["remote", "add", "origin", remote]);
    dir
}

/// An unborn skills repo (`Repo::init`, nothing committed, `sk1ll.toml`
/// staged) whose origin has a first Baseline of `files`.
fn unborn(s: &Scratch, files: &[(&str, &str)]) -> PathBuf {
    let remote = s.join("empty.git");
    sk1ll_testkit::git::init_bare(&remote);
    let seed = s.join("seed");
    Repo::init(&seed).unwrap();
    for (rel, text) in files {
        write(&seed.join(rel), text);
    }
    common::commit_all(&seed, "first baseline");
    git(&seed, &["push", "--quiet", remote.to_str().unwrap(), "main"]);
    let clone = s.join("clone");
    Repo::init(&clone).unwrap();
    git(&clone, &["remote", "add", "origin", remote.to_str().unwrap()]);
    // Keep an index too, so a reset that rewrites it is caught.
    git(&clone, &["add", "sk1ll.toml"]);
    clone
}

fn fetched<T>(dir: &Path, adopt: fn(&Path) -> Result<T>) -> Result<T> {
    Repo::open_git_only(dir)?.fetch()?;
    adopt(dir)
}

const OBSTRUCTIONS: [&str; 6] =
    ["ancestor-file", "ancestor-symlink", "target-file", "target-directory", "target-symlink", "dangling-symlink"];

/// Puts `obstruction` where the Baseline's `personal/pdf/SKILL.md` goes.
fn obstruct(obstruction: &str, clone: &Path, outside: &Path) {
    let (ancestor, target) = (clone.join("personal/pdf"), clone.join("personal/pdf/SKILL.md"));
    if !obstruction.starts_with("ancestor") {
        fs::create_dir_all(&ancestor).unwrap();
    }
    match obstruction {
        "ancestor-file" => write(&ancestor, "private"),
        "ancestor-symlink" => symlink(outside, &ancestor).unwrap(),
        "target-file" => write(&target, "private"),
        "target-directory" => write(&target.join("notes"), "private"),
        "target-symlink" => symlink(outside.join("notes"), &target).unwrap(),
        _ => symlink(outside.join("missing"), &target).unwrap(),
    }
}

fn assert_obstruction_kept(obstruction: &str, clone: &Path, outside: &Path) {
    let (ancestor, target) = (clone.join("personal/pdf"), clone.join("personal/pdf/SKILL.md"));
    match obstruction {
        "ancestor-file" => assert_eq!(read(&ancestor), "private"),
        "ancestor-symlink" => assert_eq!(fs::read_link(ancestor).unwrap(), outside),
        "target-file" => assert_eq!(read(&target), "private"),
        "target-directory" => assert_eq!(read(&target.join("notes")), "private"),
        _ => assert!(target.symlink_metadata().unwrap().is_symlink()),
    }
    assert_eq!(read(&outside.join("notes")), "outside notes");
    assert!(!outside.join("missing").exists() && !outside.join("SKILL.md").exists());
}

/// Whatever is in the way of the first Baseline, ignored or not, and
/// whether it was there before the preflight or appeared while the
/// Baseline was being published, blocks the adoption: HEAD stays unborn,
/// the index is untouched and the obstruction is left as it was.
#[test]
fn the_first_baseline_never_replaces_an_obstruction_present_or_appearing_later() {
    for appears_during_publish in [false, true] {
        for obstruction in OBSTRUCTIONS {
            for ignored in [false, true] {
                let case = format!("{obstruction}, ignored={ignored}, during publish={appears_during_publish}");
                let s = Scratch::new();
                let clone =
                    unborn(&s, &[("instructions/first.md", "published first"), ("personal/pdf/SKILL.md", "remote")]);
                let outside = s.join("outside");
                write(&outside.join("notes"), "outside notes");
                if ignored {
                    write(&clone.join(".git/info/exclude"), "personal/pdf\n");
                }
                let (head, index) = (fs::read(clone.join(".git/HEAD")).unwrap(), fs::read(clone.join(".git/index")));
                let _later = appears_during_publish.then(|| {
                    let (clone, outside) = (clone.clone(), outside.clone());
                    failpoints::once(Point::BaselinePublish, move |_| obstruct(obstruction, &clone, &outside))
                });
                if !appears_during_publish {
                    obstruct(obstruction, &clone, &outside);
                }
                let outcome = fetched(&clone, adopt_first_baseline).unwrap();
                assert!(
                    matches!(&outcome, FirstBaseline::Blocked { items, .. } if *items == [ItemRef::Skill("pdf".into())]),
                    "{case}"
                );
                assert_eq!(fs::read(clone.join(".git/HEAD")).unwrap(), head, "{case}");
                assert_eq!(fs::read(clone.join(".git/index")).unwrap(), index.unwrap(), "{case}");
                assert_eq!(rev(&clone, "HEAD").unwrap(), None, "{case}");
                // What was published before the obstruction appeared stays.
                assert_eq!(clone.join("instructions/first.md").exists(), appears_during_publish, "{case}");
                assert_obstruction_kept(obstruction, &clone, &outside);
            }
        }
    }
}

#[test]
fn an_unborn_repo_adopts_the_baseline_and_keeps_unrelated_drafts() {
    let s = Scratch::new();
    // NUL-delimited paths must survive whitespace, including newlines.
    let clone = unborn(&s, &[("personal/pdf/SKILL.md", "remote skill"), ("personal/pdf/a\nb", "attachment")]);
    write(&clone.join("notes"), "private notes");
    assert!(matches!(fetched(&clone, adopt_first_baseline).unwrap(), FirstBaseline::Adopted(_)));
    assert_eq!(read(&clone.join("personal/pdf/SKILL.md")), "remote skill");
    assert_eq!(read(&clone.join("personal/pdf/a\nb")), "attachment");
    assert_eq!(read(&clone.join("notes")), "private notes");
}

#[test]
fn a_committed_init_is_pristine_until_anything_else_is_kept() {
    let s = Scratch::new();
    let remote = server(&s);
    let fresh = |name: &str| {
        let repo = s.join(name);
        Repo::init_committed(&repo).unwrap();
        repo
    };
    check_standalone_join(&fresh("pristine"), &remote).unwrap();
    let draft = fresh("draft");
    write(&draft.join("instructions/git.md"), "draft");
    let second = fresh("second");
    git(&second, &["commit", "--quiet", "--allow-empty", "-m", "more"]);
    let branch = fresh("branch");
    git(&branch, &["branch", "kept"]);
    let removed = fresh("removed");
    git(&removed, &["rm", "--quiet", "personal/.gitkeep"]);
    let ignored = fresh("ignored");
    write(&ignored.join(".sk1ll-stage-x"), "private");
    for repo in [draft, second, branch, removed, ignored] {
        assert!(check_standalone_join(&repo, &remote).is_err(), "{}", repo.display());
    }
}

#[test]
fn a_lone_init_commit_gives_way_to_an_unrelated_first_baseline_only() {
    let s = Scratch::new();
    let remote = server(&s);
    let repo = s.join("repo");
    Repo::init_committed(&repo).unwrap();
    git(&repo, &["remote", "add", "origin", &remote]);
    let baseline = git(&repo, &["ls-remote", &remote, "refs/heads/main"]);
    assert!(matches!(fetched(&repo, adopt_first_baseline_over_init).unwrap(), Some(FirstBaseline::Adopted(b))
        if baseline.starts_with(&b)));
    assert_eq!(rev(&repo, "HEAD").unwrap(), rev(&repo, "origin/main").unwrap());
    assert!(rev(&repo, "HEAD^").unwrap().is_none());

    // Sharing history with the remote, or holding more than the starter
    // files, is an ordinary pull.
    let shared = s.join("shared");
    git(s.path(), &["clone", "--quiet", repo.to_str().unwrap(), shared.to_str().unwrap()]);
    assert!(fetched(&shared, adopt_first_baseline_over_init).unwrap().is_none());
    let other = s.join("other");
    Repo::init_committed(&other).unwrap();
    write(&other.join("instructions/git.md"), "mine");
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "--quiet", "--amend", "-m", "mine"]);
    git(&other, &["remote", "add", "origin", &remote]);
    assert!(fetched(&other, adopt_first_baseline_over_init).unwrap().is_none());
    assert!(rev(&other, "HEAD").unwrap().is_some());
}

#[test]
fn standalone_join_refuses_ignored_content_in_an_unborn_repo() {
    for linked in [false, true] {
        let s = Scratch::new();
        let remote = server(&s);
        let repo = s.join("local");
        git(s.path(), &["init", "-q", repo.to_str().unwrap()]);
        git(&repo, &["remote", "add", "origin", &remote]);
        write(&repo.join(".git/info/exclude"), "*\n");
        let outside = s.join("outside");
        fs::create_dir(&outside).unwrap();
        if linked {
            symlink(&outside, repo.join("personal")).unwrap();
        } else {
            write(&repo.join("personal/.gitkeep"), "private");
        }
        assert!(check_standalone_join(&repo, &remote).is_err());
        assert!(!outside.join(".gitkeep").exists());
    }
}

#[test]
fn an_existing_clone_on_another_branch_is_refused_before_joining() {
    let s = Scratch::new();
    let remote = server(&s);
    let dir = independent(&s, "master", &remote);
    let err = format!("{:#}", check_repo_path(&dir, &remote).unwrap_err());
    assert!(err.contains("branch `master`") && err.contains("branch -m master main"), "{err}");
    assert!(ensure_clone(&dir, &remote, &[]).is_err());
    assert_eq!(git(&dir, &["symbolic-ref", "--short", "HEAD"]), "master");
}

/// A new clone is checked out in full; an existing one is adopted only if
/// it shares the server's history, and only once the server has a Baseline.
#[test]
fn a_clone_is_checked_out_and_an_adopted_one_must_share_the_servers_history() {
    let s = Scratch::new();
    let remote = server(&s);
    let clone = s.join("clone");
    assert_eq!(ensure_clone(&clone, &remote, &[]).unwrap(), Setup::Cloned);
    assert!(clone.join("sk1ll.toml").is_file());
    assert_eq!(git(&clone, &["status", "--porcelain"]), "");
    // Commits of its own are fine.
    write(&clone.join("notes.txt"), "n\n");
    common::commit_all(&clone, "local");
    assert_eq!(ensure_clone(&clone, &remote, &[]).unwrap(), Setup::Adopted);
    check_shares_history(&clone, &[]).unwrap();

    // On `main`, but started on its own.
    let own = independent(&s, "main", &remote);
    check_repo_path(&own, &remote).unwrap();
    let err = format!("{:#}", check_shares_history(&own, &[]).unwrap_err());
    assert!(err.contains("shares no history"), "{err}");
    let empty = s.join("empty.git");
    sk1ll_testkit::git::init_bare(&empty);
    git(&own, &["remote", "set-url", "origin", empty.to_str().unwrap()]);
    assert!(check_shares_history(&own, &[]).unwrap_err().to_string().contains("no commits on `main`"));
}

/// A clone runs no filter driver, not even one git's configuration
/// names only for the clone's own git directory (`includeIf.gitdir`),
/// and a Baseline that writes a file through one is refused.
#[test]
fn a_clone_runs_no_filter_driver_a_conditional_include_names() {
    let s = Scratch::new();
    let seed = s.join("seed");
    git(s.path(), &["init", "--quiet", "-b", "main", seed.to_str().unwrap()]);
    write(&seed.join("sk1ll.toml"), "");
    write(&seed.join(".gitattributes"), "*.md filter=x\n");
    write(&seed.join("notes.md"), "notes\n");
    common::commit_all(&seed, "baseline");
    let remote = s.join("server.git");
    git(s.path(), &["clone", "--quiet", "--bare", seed.to_str().unwrap(), remote.to_str().unwrap()]);
    let marker = s.join("ran");
    let filter = s.join("filter.gitconfig");
    let cmd = format!("touch '{}'; cat", marker.display());
    write(&filter, &format!("[filter \"x\"]\n\tsmudge = {cmd}\n\tclean = {cmd}\n"));
    // Applies to any git directory in the scratch directory: the clone's,
    // not the directory it is made from.
    let global = s.join("global.gitconfig");
    write(&global, &format!("[includeIf \"gitdir:{}/**\"]\n\tpath = {}\n", s.path().display(), filter.display()));
    let env = [("GIT_CONFIG_GLOBAL".to_string(), global.to_string_lossy().into_owned())];
    let dir = s.join("clone");
    let error = ensure_clone(&dir, remote.to_str().unwrap(), &env).unwrap_err().to_string();
    assert!(error.contains("notes.md") && error.contains("filter driver"), "{error}");
    assert!(!marker.exists(), "a filter driver ran");
    assert!(!dir.exists(), "a refused clone is not moved into place");
    // The include does name the driver where git checks out.
    let plain = s.join("plain");
    let out = std::process::Command::new("git")
        .args(["clone", "--quiet", remote.to_str().unwrap(), plain.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", &global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(marker.exists(), "the test's driver is not configured as git reads it");
}
