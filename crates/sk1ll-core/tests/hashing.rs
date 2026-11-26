use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::hash::{HashCache, hash_dir};
use sk1ll_core::sync::{self, displaced};
use sk1ll_core::{ActionKind, Assignment, EngineLock, Harness, LinkMode, PlanOptions, Repo};
use sk1ll_testkit::{Home, SkillsRepo, read, write};

fn copy_harness(dir: &Path) -> BTreeMap<String, Harness> {
    [("h".into(), Harness { skills: Some(dir.to_str().unwrap().into()), mode: LinkMode::Copy, ..Harness::default() })]
        .into()
}

#[test]
fn copied_content_uses_the_same_digest_for_old_trees_and_large_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write(&dir.join("SKILL.md"), "---\nname: pdf\n---\n");
    write(&dir.join("refs/deep/é.md"), "x\n");
    write(&dir.join("refs/.git/HEAD"), "ignored\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("refs/deep", dir.join("link")).unwrap();
    let golden = if cfg!(unix) {
        "sha256:a21b4f51975084e6c1e3f116f62b3e64394781d07f70dd31ba60c9443335be85"
    } else {
        "sha256:8ed5df1f5253f60cbfa5445c2b5337744a48fd90fd08299698a2e4f2a7e59d7e"
    };
    assert_eq!(hash_dir(dir).unwrap(), golden);
    write(&dir.join(sk1ll_core::sync::PLACEMENT_MARKER), "token\n");
    write(&dir.join(".git/HEAD"), "ignored\n");
    assert_eq!(hash_dir(dir).unwrap(), golden);
    for path in [".sk1ll-state/index.json", "refs/.sk1ll-state/f", "refs/.sk1ll-placement", ".sk1ll-tmp/x"] {
        write(&dir.join(path), "content\n");
        assert_ne!(hash_dir(dir).unwrap(), golden, "{path}");
        fs::remove_file(dir.join(path)).unwrap();
    }

    let data: Vec<u8> = (0..3 * 64 * 1024 + 17).map(|i| (i % 251) as u8).collect();
    let large = tmp.path().join("large");
    fs::create_dir(&large).unwrap();
    fs::write(large.join("big.bin"), &data).unwrap();
    fs::write(large.join("exact.bin"), &data[..2 * 64 * 1024]).unwrap();
    write(&large.join("empty"), "");
    write(&large.join("refs/small.md"), "x\n");
    assert_eq!(hash_dir(&large).unwrap(), "sha256:a27c1e1e946bf1d1d48d9034199f7c3b82c06b7a29d6878090658ea110b06ead");
}

#[cfg(unix)]
#[test]
fn digests_distinguish_unix_path_bytes_and_symlink_targets() {
    use std::ffi::OsStr;
    use std::os::unix::{ffi::OsStrExt, fs::symlink};

    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    for (left, right) in [(b"a\\b".as_slice(), b"a/b".as_slice()), (b"f\xfe", b"f\xff")] {
        let (left, right) = (a.join(OsStr::from_bytes(left)), b.join(OsStr::from_bytes(right)));
        write(&left, "same\n");
        write(&right, "same\n");
        assert_ne!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
        fs::remove_file(left).unwrap();
        fs::remove_file(right).unwrap();
    }
    symlink(OsStr::from_bytes(b"t\xfe"), a.join("link")).unwrap();
    symlink(OsStr::from_bytes(b"t\xff"), b.join("link")).unwrap();
    assert_ne!(hash_dir(&a).unwrap(), hash_dir(&b).unwrap());
}

#[test]
fn sync_plans_see_rewrites_replacements_and_additions_with_an_unchanged_mtime() {
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    let source = fixture.write("personal/pdf/SKILL.md", "aaaa");
    let dest = home.path().join("skills");
    let harnesses = copy_harness(&dest);
    let assignment = Assignment::default();
    let opts = PlanOptions::default();
    let repo = fixture.open();
    let cache = HashCache::with_racy_window(Duration::ZERO);
    let plan = || sync::plan_cached(&repo, &harnesses, &assignment, opts, &cache).unwrap();
    let apply = |kind| {
        let actions = plan();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0].kind, kind);
        sync::apply(&repo, &actions[0]).unwrap();
        assert!(plan().is_empty());
    };
    apply(ActionKind::Create);

    let mtime = source.metadata().unwrap().modified().unwrap();
    for (operation, content) in [("rewrite", "bbbb"), ("replace", "cccc"), ("resize", "dddddd"), ("add", "dddddd")] {
        match operation {
            "replace" => {
                let other = fixture.write("swap", content);
                fs::rename(other, &source).unwrap();
            }
            "add" => {
                fixture.write("personal/pdf/ref.md", "added");
            }
            _ => write(&source, content),
        }
        File::options().write(true).open(&source).unwrap().set_modified(mtime).unwrap();
        apply(ActionKind::Update);
        assert_eq!(read(&dest.join("pdf/SKILL.md")), content);
    }
    assert_eq!(read(&dest.join("pdf/ref.md")), "added");
    // An unchanged tree comes from the cache until two sweeps without a
    // lookup in between forget it.
    let hashed = cache.hashed();
    assert!(plan().is_empty());
    assert_eq!(cache.hashed(), hashed, "an unchanged tree is not read again");
    cache.sweep();
    cache.sweep();
    assert!(plan().is_empty());
    assert!(cache.hashed() > hashed, "a swept tree is read again");

    let window = HashCache::with_racy_window(Duration::from_secs(3600));
    let recent = || sync::plan_cached(&repo, &harnesses, &assignment, opts, &window).unwrap();
    assert!(recent().is_empty());
    let first = window.hashed();
    assert!(recent().is_empty());
    assert!(window.hashed() > first, "a recent tree has to be read again");
}

#[test]
fn a_file_resized_during_a_sync_hash_leaves_the_copy_consistent_with_the_source() {
    let home = Home::new();
    let _entered = home.enter();
    let fixture = SkillsRepo::new();
    fixture.write("personal/pdf/SKILL.md", "PDFs.\n");
    let source = fixture.write("personal/pdf/data", &"x".repeat(80_000));
    let dest = home.path().join("skills");
    let harnesses = copy_harness(&dest);
    let assignment = Assignment::default();
    let opts = PlanOptions::default();
    let repo = fixture.open();
    let plan = |repo: &Repo| sync::plan(repo, &harnesses, &assignment, opts).unwrap();
    sync::apply(&repo, &plan(&repo)[0]).unwrap();
    for length in [180_000, 3, 0] {
        let live = source.clone();
        let changed = "y".repeat(length);
        let mut hit = false;
        let actions = failpoints::scoped(
            |point, path| {
                if point == Point::HashMidRead && path == live && !hit {
                    hit = true;
                    write(path, &changed);
                }
                Action::Continue
            },
            || plan(&repo),
        );
        assert!(hit);
        assert_eq!(actions.len(), 1, "{actions:?}");
        sync::apply(&repo, &actions[0]).unwrap();
        assert_eq!(read(&dest.join("pdf/data")), changed);
        assert!(plan(&fixture.open()).is_empty());
    }
}

#[cfg(unix)]
#[test]
fn recovery_keeps_payloads_changed_after_their_last_check() {
    use rustix::fs::{XattrFlags, lremovexattr, lsetxattr};
    use rustix::io::Errno;
    use std::os::unix::fs::PermissionsExt;

    let home = Home::new();
    let _entered = home.enter();
    for case in ["unchanged", "content", "addition", "mode", "xattr"] {
        let fixture = SkillsRepo::new();
        let placement = tempfile::tempdir().unwrap();
        let harnesses = copy_harness(placement.path());
        let assignment = Assignment::default();
        let opts = PlanOptions::default();
        let repo = fixture.open();
        repo.create_item("skill", "pdf", "PDF").unwrap();
        let source = fixture.write("personal/pdf/SKILL.md", "original\n");
        let sync = || {
            for action in sync::plan(&repo, &harnesses, &assignment, opts).unwrap() {
                sync::apply(&repo, &action).unwrap();
            }
        };
        sync();
        write(&source, "upstream\n");
        sync();
        let entry = displaced::list().unwrap().into_iter().find(|e| e.dest == placement.path().join("pdf")).unwrap();
        let old = entry.path.join("SKILL.md");
        // An attribute there beforehand keeps the entry anyway; this one
        // only comes after the last check.
        if case == "xattr" {
            match lsetxattr(&old, "user.sk1ll-test", b"1", XattrFlags::empty()) {
                Err(Errno::OPNOTSUPP | Errno::PERM) => continue,
                result => result.unwrap(),
            }
            lremovexattr(&old, "user.sk1ll-test").unwrap();
        }
        let expected = entry.path.clone();
        let mut hit = false;
        let adopt = || {
            let lock = EngineLock::acquire(repo.root()).unwrap();
            displaced::adopt(&repo, &entry.id, &lock).unwrap()
        };
        failpoints::scoped(
            |point, path| {
                if point == Point::HashBeforeRemoval && path == expected {
                    hit = true;
                    match case {
                        "content" => write(&old, "later edit\n"),
                        "addition" => write(&expected.join("new"), "later addition\n"),
                        "mode" => fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap(),
                        "xattr" => lsetxattr(&old, "user.sk1ll-test", b"2", XattrFlags::empty()).unwrap(),
                        _ => (),
                    }
                }
                Action::Continue
            },
            adopt,
        );
        assert!(hit, "{case}");
        assert_eq!(read(&source), "original\n", "{case}");
        if case == "unchanged" {
            assert!(displaced::get(&entry.id).is_err());
        } else {
            assert!(displaced::get(&entry.id).unwrap().changed, "{case}");
            let kept = if case == "addition" { PathBuf::from("new") } else { PathBuf::from("SKILL.md") };
            assert!(entry.path.join(kept).exists(), "{case}");
        }
    }
}
