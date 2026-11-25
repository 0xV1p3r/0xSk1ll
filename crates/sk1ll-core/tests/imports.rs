//! Imports through `Repo`: resolving sources, adding, updating, forking
//! and removing vendored skills, and recovering interrupted installs.

mod common;

use common::{Scratch, git, write};
use sk1ll_core::failpoints::{self, Action, Point};
use sk1ll_core::imports::*;
use sk1ll_core::source;
use sk1ll_core::{Import, Repo};
use sk1ll_testkit::SkillsRepo;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// A second version of `skills/pdf/SKILL.md`.
const V2: &str = "---\ndescription: v2\n---\n";

#[test]
fn remove_rejects_unsafe_paths_before_mutating() {
    let src = Source::new();
    let root = src.s.join("repo");
    write(&root.join("sk1ll.toml"), "[import.'../personal']\nsource = 'unused'\n");
    let mut repo = src.repo();
    fs::write(repo.personal_dir().join("keep.txt"), "personal content").unwrap();
    let before = fs::read(repo.root().join("sk1ll.toml")).unwrap();
    for name in ["../personal", "a..b", "a/b", "a\\b", "-bad", "_bad"] {
        assert!(repo.remove_import(name).is_err());
        assert!(repo.create_item("skill", name, "").is_err());
    }
    assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), before);
    assert_eq!(fs::read_to_string(repo.personal_dir().join("keep.txt")).unwrap(), "personal content");
    assert!(repo.manifest.import.contains_key("../personal"));
}

#[cfg(unix)]
#[test]
fn remove_rejects_symlink_target_and_parent() {
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let before = fs::read(repo.root().join("sk1ll.toml")).unwrap();
    let target = repo.imported_dir().join("pdf");
    fs::rename(&target, repo.root().join("saved")).unwrap();
    std::os::unix::fs::symlink(repo.root().join("saved"), &target).unwrap();
    assert!(repo.remove_import("pdf").is_err());
    fs::remove_file(&target).unwrap();
    fs::rename(repo.root().join("saved"), &target).unwrap();
    fs::rename(repo.imported_dir(), repo.root().join("saved-imported")).unwrap();
    std::os::unix::fs::symlink(repo.root().join("saved-imported"), repo.imported_dir()).unwrap();
    assert!(repo.remove_import("pdf").is_err());
    assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), before);
    assert!(repo.manifest.import.contains_key("pdf"));
    assert!(target.join("SKILL.md").exists());
}

/// A local source repo (`origin`) with skills at `skills/pdf`,
/// `skills/web` and `skills/web/nested`, on `main` and a slashed branch
/// `feature/x`, and a skills repo (`repo`) to import them into.
struct Source {
    s: Scratch,
}

impl Source {
    fn new() -> Self {
        let s = Scratch::new();
        let origin = s.join("origin");
        let skills = [("skills/pdf", "Work with PDFs"), ("skills/web", "Browse"), ("skills/web/nested", "")];
        for (dir, description) in skills {
            write(&origin.join(dir).join("SKILL.md"), &format!("---\nname: x\ndescription: {description}\n---\n"));
        }
        write(&origin.join("README.md"), "skills\n");
        git(&origin, &["init", "--quiet", "-b", "main"]);
        common::commit_all(&origin, "seed");
        git(&origin, &["branch", "feature/x"]);
        SkillsRepo::init(s.join("repo"));
        Self { s }
    }

    fn origin(&self) -> String {
        self.dir().to_string_lossy().into_owned()
    }

    fn dir(&self) -> PathBuf {
        self.s.join("origin")
    }

    fn repo(&self) -> Repo {
        Repo::open(self.s.join("repo")).unwrap()
    }

    /// Commits `text` at `rel` in the source.
    fn release(&self, rel: &str, text: &str) {
        write(&self.dir().join(rel), text);
        common::commit_all(&self.dir(), rel);
    }

    /// Adds `skills/pdf` as the import `pdf`.
    fn add_pdf(&self, repo: &mut Repo) -> UpdateReport {
        self.try_add_pdf(repo).unwrap()
    }

    fn try_add_pdf(&self, repo: &mut Repo) -> sk1ll_core::Result<UpdateReport> {
        repo.add_link(None, &self.origin(), Some("skills/pdf"), None, None)
    }
}

/// `imported/pdf/SKILL.md` as installed.
fn installed(repo: &Repo) -> String {
    sk1ll_testkit::read(&repo.imported_dir().join("pdf/SKILL.md"))
}

fn paths(r: &ResolvedSource) -> Vec<&str> {
    r.candidates.iter().map(|c| c.path.as_str()).collect()
}

#[test]
fn preview_reports_warnings_from_git_blobs() {
    let src = Source::new();
    let origin = src.dir();
    let skill = origin.join("skills/pdf");
    fs::write(skill.join("run"), "#!/bin/sh\n").unwrap();
    fs::write(skill.join("test.PY"), "print(1)\n").unwrap();
    fs::write(skill.join("big"), vec![0xff; 1024 * 1024 + 1]).unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["update-index", "--chmod=+x", "skills/pdf/run"]);
    git(&origin, &["commit", "-qm", "files"]);
    let resolved = src.repo().resolve_source(&src.origin(), Some("skills/pdf"), None).unwrap();
    let candidate = &resolved.candidates[0];
    assert_eq!(candidate.total_bytes, 1024 * 1024 + 1 + 10 + 9 + fs::metadata(skill.join("SKILL.md")).unwrap().len());
    let warnings: Vec<_> = candidate.warnings.iter().map(|w| (w.path.as_str(), w.kinds.as_slice())).collect();
    use WarningKind::{Binary, Executable, Large, Script};
    assert_eq!(warnings, [("big", &[Binary, Large][..]), ("run", &[Executable, Script]), ("test.PY", &[Script])]);
}

#[test]
fn update_preview_lists_commits_and_only_files_in_the_skill() {
    let src = Source::new();
    let mut repo = src.repo();
    let added = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("main"), None).unwrap();
    src.release("README.md", "unrelated\n");
    src.release("skills/pdf/new.txt", "new\n");
    let preview = fetch_changes(repo.root(), repo.update_targets(Some("pdf"), true).unwrap(), None).unwrap();
    assert_eq!(preview.locked, added.new_commit);
    assert_eq!(preview.commits.len(), 2);
    assert_eq!(preview.commits[0].subject, "skills/pdf/new.txt");
    assert!(preview.upstream.starts_with(&preview.commits[0].sha));
    assert_eq!(preview.commits[0].date.len(), 10);
    assert_eq!(preview.files, ["skills/pdf/new.txt"]);
    assert!(!preview.truncated);
    assert_eq!(repo.import_status().unwrap()[0].commit.as_ref(), Some(&added.new_commit));
    assert!(!repo.imported_dir().join("pdf/new.txt").exists());
}

#[test]
fn resolving_lists_skills_without_writing_the_repo() {
    let src = Source::new();
    let mut repo = src.repo();
    let all = repo.resolve_source(&src.origin(), None, None).unwrap();
    assert_eq!(all.source, src.origin());
    assert_eq!(all.git_ref, "HEAD");
    assert_eq!(all.commit.len(), 40);
    assert_eq!(paths(&all), ["skills/pdf", "skills/web", "skills/web/nested"]);
    let pdf = &all.candidates[0];
    assert_eq!((pdf.default_name.as_str(), pdf.description.as_deref()), ("pdf", Some("Work with PDFs")));
    assert!(!pdf.taken);
    assert_eq!(all.candidates[2].description, None);
    assert_eq!(repo.draft().unwrap(), Vec::new(), "resolving changes nothing in the repo");

    // A skill directory is the only candidate, even with skills below it.
    let web = repo.resolve_source(&src.origin(), Some("skills/web/"), Some("feature/x")).unwrap();
    assert_eq!((paths(&web), web.git_ref.as_str()), (vec!["skills/web"], "feature/x"));
    assert_eq!(paths(&repo.resolve_source(&src.origin(), Some("skills"), None).unwrap()).len(), 3);
    let missing = repo.resolve_source(&src.origin(), Some("nope"), None).unwrap_err().to_string();
    assert_eq!(missing, "there is no `nope` at `HEAD`");
    let file = repo.resolve_source(&src.origin(), Some("README.md"), None).unwrap_err().to_string();
    assert!(file.starts_with("`README.md` is a file, not a folder"), "{file}");

    fs::create_dir_all(repo.personal_dir().join("pdf")).unwrap();
    repo.add_link(Some("browse"), &src.origin(), Some("skills/web"), Some("main"), None).unwrap();
    let again = repo.resolve_source(&src.origin(), Some("skills"), None).unwrap();
    let taken: Vec<_> = again.candidates.iter().map(|c| (c.default_name.as_str(), c.taken)).collect();
    assert_eq!(taken, [("pdf", true), ("web", false), ("nested", false)]);
}

#[test]
fn adding_a_link_needs_exactly_one_skill() {
    let src = Source::new();
    let mut repo = src.repo();
    let err = repo.add_link(None, &src.origin(), None, None, None).unwrap_err().to_string();
    assert!(err.contains("has 3 skills: skills/pdf, skills/web, skills/web/nested"), "{err}");
    assert!(err.contains("--path") && err.contains("sk1ll add --list"), "{err}");
    let err = repo.add_link(None, &src.origin(), Some("docs"), None, None).unwrap_err().to_string();
    assert_eq!(err, "there is no `docs` at `HEAD`");
    assert!(repo.manifest.import.is_empty() && !repo.imported_dir().join("pdf").exists());

    let r = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("feature/x"), None).unwrap();
    assert_eq!(r.name, "pdf");
    assert!(repo.imported_dir().join("pdf/SKILL.md").is_file());
    let import = &repo.manifest.import["pdf"];
    assert_eq!((import.path.as_str(), import.git_ref.as_str()), ("skills/pdf", "feature/x"));
    assert_eq!(repo.lock.skills["pdf"].commit, r.new_commit);
    let reopened = src.repo();
    assert_eq!(reopened.manifest.import["pdf"], *import);
    assert_eq!(reopened.import_status().unwrap()[0].state, ImportState::Clean);

    let err = repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err().to_string();
    assert!(err.contains("import `pdf` already exists"), "{err}");
}

/// Anything already at `imported/<name>` that isn't a recorded import is
/// never replaced by a new import (updates of recorded ones still are).
#[test]
fn adding_never_replaces_an_unrecorded_imported_dir() {
    let src = Source::new();
    let mut repo = src.repo();
    let orphan = repo.imported_dir().join("pdf");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("notes.md"), "mine\n").unwrap();
    let err = repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err().to_string();
    assert!(err.contains("not a recorded import"), "{err}");
    let import = Import { id: None, source: src.origin(), path: "skills/pdf".into(), git_ref: "HEAD".into() };
    assert!(repo.add_import("pdf", import.clone()).is_err());
    assert_eq!(fs::read_to_string(orphan.join("notes.md")).unwrap(), "mine\n");
    assert!(repo.manifest.import.is_empty());
    assert!(repo.resolve_source(&src.origin(), Some("skills/pdf"), None).unwrap().candidates[0].taken);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(repo.root().join("missing"), repo.imported_dir().join("web")).unwrap();
        let err = repo.add_import("web", Import { path: "skills/web".into(), ..import.clone() }).unwrap_err();
        assert!(err.to_string().contains("not a recorded import"), "{err}");
    }

    fs::remove_dir_all(&orphan).unwrap();
    repo.add_import("pdf", import).unwrap();
    fs::write(repo.imported_dir().join("pdf/SKILL.md"), "edited").unwrap();
    repo.update(Some("pdf"), true).unwrap();
    assert_ne!(installed(&repo), "edited");
}

/// What a preview showed is what gets added, even if upstream moved on.
#[test]
fn adding_a_previewed_commit_vendors_exactly_that_commit() {
    let src = Source::new();
    let mut repo = src.repo();
    let preview = repo.resolve_source(&src.origin(), Some("skills/pdf"), Some("main")).unwrap();
    src.release("skills/pdf/SKILL.md", V2);

    let r = repo.add_link(None, &src.origin(), Some("skills/pdf"), Some("main"), Some(&preview.commit)).unwrap();
    assert_eq!(r.new_commit, preview.commit);
    let text = installed(&repo);
    assert!(text.contains("Work with PDFs"), "{text}");
    assert_eq!(repo.manifest.import["pdf"].git_ref, "main");
    assert_eq!(repo.lock.skills["pdf"].commit, preview.commit);

    let zero = "0000000000000000000000000000000000000000";
    for (bad, why) in [(zero, "is not in"), ("main", "not a SHA"), ("--all", "not a SHA")] {
        let err = repo.add_link(Some("x"), &src.origin(), Some("skills/web"), None, Some(bad)).unwrap_err();
        assert!(err.to_string().contains(why), "{bad}: {err}");
    }
    assert!(!repo.manifest.import.contains_key("x"));
}

#[cfg(unix)]
#[test]
fn fork_rejects_symlinked_paths_before_touching_the_import() {
    use std::os::unix::fs::symlink;
    for linked in ["entry", "imported", "personal"] {
        let src = Source::new();
        let mut repo = src.repo();
        src.add_pdf(&mut repo);
        let dir = match linked {
            "entry" => repo.imported_dir().join("pdf"),
            "imported" => repo.imported_dir(),
            _ => repo.personal_dir(),
        };
        let outside = src.s.join("outside");
        fs::rename(&dir, &outside).unwrap();
        symlink(&outside, &dir).unwrap();
        let manifest = fs::read(repo.root().join("sk1ll.toml")).unwrap();
        assert!(repo.fork("pdf").is_err(), "{linked}");
        assert!(dir.symlink_metadata().unwrap().is_symlink());
        assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), manifest);
        assert!(!outside.join(".sk1ll-id").exists());
        assert!(!outside.join("pdf/.sk1ll-id").exists());
    }
}

#[test]
fn a_local_vendor_sidecar_edit_blocks_update() {
    let src = Source::new();
    let mut repo = src.repo();
    let leaf = "skills/pdf/.sk1ll-id";
    src.release(leaf, "upstream\n");
    src.add_pdf(&mut repo);
    let path = repo.imported_dir().join("pdf/.sk1ll-id");
    assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Clean);
    fs::write(&path, "local\n").unwrap();
    assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Modified);
    assert!(repo.update(Some("pdf"), false).unwrap_err().to_string().contains("locally modified"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "local\n");
    repo.update(Some("pdf"), true).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "upstream\n");
}

#[test]
fn fork_removes_inline_import_and_requires_both_commit_and_restore_sides() {
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let import = &repo.manifest.import["pdf"];
    let id = import.id.clone().unwrap();
    let manifest = format!("import = {{ pdf = {{ source = '{}', id = '{id}' }} }}\n", import.source);
    fs::write(repo.root().join("sk1ll.toml"), manifest).unwrap();
    let skill = sk1ll_core::ItemRef::Skill("pdf".into());
    let items = [sk1ll_core::ItemRef::Manifest, skill];
    repo.commit(&items, "Add inline import", None).unwrap();
    repo.fork("pdf").unwrap();
    assert!(sk1ll_core::Manifest::load(repo.root()).unwrap().import.is_empty());
    assert_eq!(repo.skills().unwrap()[0].id.as_ref(), Some(&id));
    for item in &items {
        let only = std::slice::from_ref(item);
        for error in [repo.commit(only, "One side", None).unwrap_err(), repo.restore(only).unwrap_err()] {
            assert!(
                error.to_string().contains("manifest, skill:pdf") || error.to_string().contains("skill:pdf, manifest"),
                "{error}"
            );
        }
    }
    repo.commit(&items, "Fork", None).unwrap();
}

#[test]
fn add_records_an_id_and_fork_carries_it_into_the_personal_skill() {
    let src = Source::new();
    let mut repo = src.repo();
    src.release("skills/pdf/.sk1ll-id", "upstream file\n");
    src.add_pdf(&mut repo);

    let mut repo = src.repo();
    let id = repo.manifest.import["pdf"].id.clone().unwrap();
    assert!(ulid::Ulid::from_string(&id.to_string()).is_ok());
    assert_eq!(repo.skills().unwrap()[0].id.as_ref(), Some(&id));
    assert_eq!(fs::read(repo.imported_dir().join("pdf/.sk1ll-id")).unwrap(), b"upstream file\n");

    repo.fork("pdf").unwrap();
    let skill = repo.skills().unwrap().remove(0);
    assert_eq!(skill.origin, sk1ll_core::Origin::Personal);
    assert_eq!(skill.id.as_ref(), Some(&id));
    assert_eq!(repo.skills().unwrap().into_iter().find(|s| s.name == "pdf").unwrap().id, Some(id));
    assert!(!repo.manifest.import.contains_key("pdf"));
}

/// Bad input is refused before anything is fetched.
#[test]
fn invalid_input_is_refused_before_fetching() {
    let src = Source::new();
    let mut repo = src.repo();
    let cache = src.s.home.cache_dir();
    let o = src.origin();
    assert!(repo.resolve_source(&o, None, Some("main..other")).is_err());
    assert!(repo.resolve_source(&o, Some("skills/../x"), None).is_err());
    assert!(repo.add_link(Some("bad name"), &o, Some("skills/pdf"), None, None).is_err());
    assert!(repo.add_link(None, &o, Some("skills/pdf"), Some("-x"), None).is_err());
    assert!(repo.add_link(None, &o, Some("skills/pdf"), None, Some("HEAD~1")).is_err());
    assert!(repo.add_link(None, &format!("{o}\n--upload-pack=x"), None, None, None).is_err());
    // Pasted GitHub links that lead nowhere safe, with what the user is told.
    for (link, expected) in [
        ("-x", "invalid source `-x`"),
        ("https://github.com/o", "is not a link to a GitHub repo, folder or file"),
        ("https://github.com/o//r", "is not a link to a GitHub repo, folder or file"),
        ("https://github.com/o$/r", "is not a link to a GitHub repo, folder or file"),
        ("https://github.com/o/r/commits/main", "is not a link to a GitHub repo, folder or file"),
        ("https://github.com/o/r/tree/main/%zz", "bad percent-encoding"),
        ("https://github.com/o/r/tree/main/a/../b", "unsafe path segment `..`"),
        ("https://github.com/o/r/tree/main/a%2F..%2Fb", "unsafe path segment `..`"),
        ("https://github.com/o/r/blob/main/skills/x.md", "blob links must point at a SKILL.md"),
    ] {
        let error = repo.resolve_source(link, None, None).unwrap_err().to_string();
        assert!(error.contains(expected), "{link}: {error}");
    }
    assert!(!cache.exists(), "nothing was cloned");
}

/// Staging (and set-aside) directories left in `imported/`.
fn stages(repo: &Repo) -> usize {
    fs::read_dir(repo.imported_dir()).unwrap().filter(|e| source::is_staging_path(&e.as_ref().unwrap().path())).count()
}

fn smudge(command: &str) -> impl Drop + use<> {
    failpoints::git_env(&[
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "filter.probe.smudge"),
        ("GIT_CONFIG_VALUE_0", command),
        ("GIT_CONFIG_KEY_1", "filter.probe.required"),
        ("GIT_CONFIG_VALUE_1", "true"),
    ])
}

/// The local phase needs no network: it installs from the cache even once
/// the source is gone, and re-checks names against the repo as it is now.
#[test]
fn fetched_links_are_vendored_offline_and_rechecked() {
    let src = Source::new();
    let mut repo = src.repo();
    let pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
    let mut web = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
    web.stage().unwrap();
    assert_eq!(stages(&repo), 1);
    fs::remove_dir_all(src.s.join("origin")).unwrap();
    // Something took the name between the fetch and the add.
    fs::create_dir_all(repo.personal_dir().join("pdf")).unwrap();
    assert!(repo.with_taken(pdf.resolved.clone()).candidates[0].taken);
    let err = repo.add_fetched(None, pdf).unwrap_err().to_string();
    assert!(err.contains("personal skill named `pdf`"), "{err}");
    let commit = web.resolved.commit.clone();
    let r = repo.add_fetched(None, web).unwrap();
    assert_eq!((r.name.as_str(), r.new_commit.as_str()), ("web", commit.as_str()));
    assert!(repo.imported_dir().join("web/SKILL.md").is_file());
    assert!(repo.import_status().unwrap().iter().all(|entry| entry.state == ImportState::Clean));
    assert_eq!(stages(&repo), 0, "installed or dropped, nothing staged is left behind");
}

#[test]
fn fetched_updates_apply_offline_unless_the_import_changed() {
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let release = |v: &str| src.release("skills/pdf/SKILL.md", &format!("---\ndescription: {v}\n---\n"));
    release("v2");

    // Edited locally after the fetch: refused unless forced.
    let fetched = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
    fs::write(repo.imported_dir().join("pdf/SKILL.md"), "edited").unwrap();
    assert!(repo.apply_updates(fetched, false).unwrap_err().to_string().contains("locally modified"));
    let fetched = fetch_updates(repo.root(), repo.update_targets(None, true).unwrap()).unwrap();
    let r = repo.apply_updates(fetched, true).unwrap();
    assert!(r[0].changed && installed(&repo).contains("v2"));

    // A slow update (fetched v3) must not undo a quicker one (v4) that
    // finished while it was fetching.
    release("v3");
    let slow = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
    release("v4");
    repo.update(Some("pdf"), false).unwrap();
    let err = repo.apply_updates(slow, false).unwrap_err().to_string();
    assert!(err.contains("updated by another operation while fetching"), "{err}");
    assert!(installed(&repo).contains("v4"));

    let gone = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
    repo.remove_import("pdf").unwrap();
    let err = repo.apply_updates(gone, true).unwrap_err().to_string();
    assert!(err.contains("changed while it was fetched"), "{err}");
    assert!(!repo.imported_dir().join("pdf").exists());
    assert_eq!(stages(&repo), 0);
}

/// Checkout runs smudge filters (Git LFS fetches there), so it happens
/// while staging or fetching an update, in the network phase. Installing
/// runs no git at all: it works with a filter that would fail, and
/// installs what was staged.
#[test]
fn smudge_filters_run_in_the_network_phase_never_while_installing() {
    let src = Source::new();
    let mut repo = src.repo();
    src.release("skills/pdf/.gitattributes", "SKILL.md filter=probe\n");
    let log = src.s.join("smudge.log");
    let probe = format!("sh -c 'echo ran >> \"{}\"; sed s/PDFs/smudged/'", log.display());
    let runs = || fs::read_to_string(&log).map_or(0, |t| t.lines().count());

    let working = smudge(&probe);
    let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
    pdf.stage().unwrap();
    let staged = runs();
    assert!(staged > 0, "the filter ran while staging");
    drop(working);
    let failing = smudge("false");
    repo.add_fetched(None, pdf).unwrap();
    assert_eq!(runs(), staged, "installing ran no filter");
    assert!(installed(&repo).contains("Work with smudged"), "{}", installed(&repo));
    assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Clean);
    drop(failing);

    src.release("skills/pdf/SKILL.md", V2);
    let working = smudge(&probe);
    let fetched = fetch_updates(repo.root(), repo.update_targets(None, false).unwrap()).unwrap();
    assert!(runs() > staged, "the filter ran while fetching");
    drop(working);
    let _failing = smudge("false");
    repo.apply_updates(fetched, false).unwrap();
    assert!(installed(&repo).contains("v2"));
}

/// Something that appears at `imported/<name>` while an import is being
/// staged (a slow smudge filter) or installed is never removed, through
/// `add_link` or the legacy `add_import`.
#[test]
fn adding_never_removes_what_appeared_while_staging() {
    for legacy in [false, true] {
        let src = Source::new();
        let mut repo = src.repo();
        src.release("skills/pdf/.gitattributes", "SKILL.md filter=probe\n");
        let dest = repo.imported_dir().join("pdf");
        let appear = format!("sh -c 'mkdir -p \"{0}\" && echo mine > \"{0}/notes.md\"; cat'", dest.display());
        let filter = smudge(&appear);
        let err = if legacy {
            let import = Import { id: None, source: src.origin(), path: "skills/pdf".into(), git_ref: "HEAD".into() };
            repo.add_import("pdf", import).unwrap_err()
        } else {
            repo.add_link(None, &src.origin(), Some("skills/pdf"), None, None).unwrap_err()
        };
        assert!(err.to_string().contains("not a recorded import"), "{err}");
        assert_eq!(fs::read_to_string(dest.join("notes.md")).unwrap(), "mine\n");
        assert!(repo.manifest.import.is_empty() && repo.lock.skills.is_empty());
        drop(filter);
    }

    // Appearing after the last check: the install itself refuses.
    let src = Source::new();
    let mut repo = src.repo();
    let mut late = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
    late.stage().unwrap();
    let _appear = failpoints::once(Point::SourceInstall, |dest| write(&dest.join("notes.md"), "mine\n"));
    let err = repo.add_fetched(Some("late"), late).unwrap_err().to_string();
    assert!(err.contains("appeared while the import was being added"), "{err}");
    assert_eq!(fs::read_to_string(repo.imported_dir().join("late/notes.md")).unwrap(), "mine\n");
    assert_eq!(stages(&repo), 0);
}

/// A failed install (say EXDEV) leaves the previous version in place.
#[test]
fn a_failed_update_install_keeps_the_previous_import() {
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let before = (installed(&repo), repo.lock.clone());
    src.release("skills/pdf/SKILL.md", V2);

    let fail = failpoints::fail_once(Point::SourceInstall, std::io::ErrorKind::CrossesDevices);
    assert!(repo.update(Some("pdf"), false).is_err());
    drop(fail);
    assert_eq!(installed(&repo), before.0);
    assert_eq!(repo.lock.skills, before.1.skills);
    assert_eq!(Repo::open(repo.root()).unwrap().import_status().unwrap()[0].state, ImportState::Clean);
    assert_eq!(stages(&repo), 0);
    repo.update(Some("pdf"), false).unwrap();
    assert!(installed(&repo).contains("v2"));
}

/// With `imported/` on another directory (or file system), staging
/// happens there, next to the destination.
#[cfg(unix)]
#[test]
fn staging_happens_where_imported_really_is() {
    let src = Source::new();
    let mut repo = src.repo();
    let elsewhere = src.s.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::remove_dir_all(repo.imported_dir()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, repo.imported_dir()).unwrap();
    let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
    pdf.stage().unwrap();
    let staged = fs::read_dir(&elsewhere).unwrap().filter_map(|e| e.ok()).count();
    assert_eq!(staged, 1, "staged next to the real destination");
    repo.add_fetched(None, pdf).unwrap();
    assert!(elsewhere.join("pdf/SKILL.md").is_file());
    repo.update(Some("pdf"), true).unwrap();
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1, "only the import is left");
}

/// A worktree whose `.gitignore` doesn't mention staging directories
/// still never shows one in its draft, and discovery skips them.
#[test]
fn staging_never_shows_in_the_draft_or_as_a_skill() {
    let src = Source::new();
    let main = src.s.join("repo");
    fs::write(main.join(".gitignore"), "notes/\n").unwrap();
    common::commit_all(&main, "ignore notes");
    let wt = src.s.join("wt");
    git(&main, &["worktree", "add", "--quiet", wt.to_str().unwrap()]);
    let repo = Repo::open(&wt).unwrap();
    assert!(repo.root().join(".git").is_file());

    let mut pdf = fetch_link(repo.root(), &src.origin(), Some("skills/pdf"), None, None).unwrap();
    pdf.stage().unwrap();
    assert_eq!(stages(&repo), 1);
    let exclude = fs::read_to_string(main.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.matches(".sk1ll-stage-*").count(), 1, "{exclude}");
    assert_eq!(repo.draft().unwrap(), Vec::new());
    // Even without the exclude line, draft and discovery skip staging.
    fs::write(main.join(".git/info/exclude"), "").unwrap();
    fs::create_dir_all(repo.imported_dir().join(".sk1ll-stage-x")).unwrap();
    fs::write(repo.imported_dir().join(".sk1ll-stage-x/SKILL.md"), "staged").unwrap();
    assert_eq!(repo.draft().unwrap(), Vec::new());
    assert!(repo.skills().unwrap().is_empty());
    // Staging again restores the line, once.
    let mut web = fetch_link(repo.root(), &src.origin(), Some("skills/web"), None, None).unwrap();
    web.stage().unwrap();
    web.stage().unwrap();
    assert_eq!(fs::read_to_string(main.join(".git/info/exclude")).unwrap(), ".sk1ll-stage-*\n");
}

/// Install and rollback both fail: the error says where the previous
/// version is, and recovery (before the next sync) puts it back, so a
/// sync keeps its harness entry.
#[test]
fn a_failed_rollback_is_recovered_before_the_next_sync() {
    use sk1ll_core::{ActionKind, PlanOptions, sync};
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let skills = src.s.join("harness");
    let machine = format!("id = 't'\n[harnesses.h]\nskills = '{}'\nmode = 'copy'\n", skills.display());
    fs::write(src.s.join("machine.toml"), machine).unwrap();
    let harnesses = sk1ll_core::MachineConfig::load(&src.s.join("machine.toml")).unwrap().harnesses();
    let harnesses: std::collections::BTreeMap<_, _> = harnesses.into_iter().filter(|(n, _)| n == "h").collect();
    let assignment = sk1ll_core::Assignment::default();
    for a in sync::plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap() {
        sync::apply(&repo, &a).unwrap();
    }
    assert!(skills.join("pdf/SKILL.md").is_file());
    let before = installed(&repo);
    src.release("skills/pdf/SKILL.md", V2);

    let fail = (
        failpoints::fail_once(Point::SourceInstall, std::io::ErrorKind::CrossesDevices),
        failpoints::fail_once(Point::SourceRestore, std::io::ErrorKind::Other),
    );
    let err = repo.update(Some("pdf"), false).unwrap_err().to_string();
    drop(fail);
    let backup = repo.imported_dir().join(".sk1ll-stage-old-pdf");
    assert!(
        err.contains(&backup.display().to_string()) && err.contains("injected at failpoint SourceRestore"),
        "{err}"
    );
    assert!(!repo.imported_dir().join("pdf").exists() && backup.is_dir());

    assert_eq!(repo.recover_imports().unwrap(), ["pdf"]);
    assert_eq!(installed(&repo), before);
    let actions = sync::plan(&repo, &harnesses, &assignment, PlanOptions::default()).unwrap();
    assert!(actions.iter().all(|a| a.kind != ActionKind::Remove), "{actions:?}");
    assert!(repo.recover_imports().unwrap().is_empty());

    // A backup left next to an installed import (a crash after the
    // install) is just deleted.
    fs::create_dir_all(&backup).unwrap();
    assert!(repo.recover_imports().unwrap().is_empty());
    assert!(!backup.exists() && repo.imported_dir().join("pdf/SKILL.md").is_file());
}

/// The exclude line is appended, never rewriting what's there (even
/// bytes that aren't UTF-8), and only once with stages racing.
#[test]
fn the_exclude_line_is_only_ever_appended_once() {
    let src = Source::new();
    let repo = src.repo();
    let exclude = repo.root().join(".git/info/exclude");
    fs::write(&exclude, b"# mine\n\xff\xfe not utf-8").unwrap();
    let racers: Vec<_> = (0..8)
        .map(|_| {
            let root = repo.root().to_path_buf();
            let origin = src.origin();
            let dirs = src.s.home.dirs();
            std::thread::spawn(move || {
                let _dirs = failpoints::dirs(dirs);
                let _git = failpoints::git_env(sk1ll_testkit::git::ENV);
                fetch_link(&root, &origin, Some("skills/pdf"), None, None).unwrap().stage().unwrap();
            })
        })
        .collect();
    for r in racers {
        r.join().unwrap();
    }
    assert_eq!(fs::read(&exclude).unwrap(), b"# mine\n\xff\xfe not utf-8\n.sk1ll-stage-*\n");
}

/// A copy of the flat directory `from` at `to`.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

/// Recovery never deletes a kept previous version unless what is
/// installed is exactly what the lockfile records.
#[test]
fn recovery_never_deletes_the_backup_on_doubt() {
    let src = Source::new();
    let mut repo = src.repo();
    src.add_pdf(&mut repo);
    let dest = repo.imported_dir().join("pdf");
    let backup = repo.imported_dir().join(".sk1ll-stage-old-pdf");
    let recorded = repo.imported_dir().join("recorded");
    copy_dir(&dest, &recorded);
    let restore = |at_dest: &dyn Fn(&Path)| {
        let _ = fs::remove_dir_all(&dest).or_else(|_| fs::remove_file(&dest));
        let _ = fs::remove_dir_all(&backup);
        copy_dir(&recorded, &backup);
        at_dest(&dest);
    };
    let empty = |d: &Path| fs::create_dir_all(d).unwrap();
    let user = |d: &Path| {
        fs::create_dir_all(d).unwrap();
        fs::write(d.join("notes.md"), "mine").unwrap();
    };
    let file = |d: &Path| fs::write(d, "a file").unwrap();
    let partial = |d: &Path| {
        copy_dir(&recorded, d);
        fs::remove_file(d.join("SKILL.md")).unwrap();
    };
    // A crash after the install, before the lock was written.
    let unrecorded = |d: &Path| {
        copy_dir(&recorded, d);
        fs::write(d.join("SKILL.md"), "---\ndescription: v2\n---\n").unwrap();
    };
    type Make<'a> = &'a dyn Fn(&Path);
    let cases: [(&str, Make); 5] =
        [("empty", &empty), ("user", &user), ("file", &file), ("partial", &partial), ("unrecorded", &unrecorded)];
    for (case, at_dest) in cases {
        restore(at_dest);
        let err = repo.recover_imports().unwrap_err().to_string();
        assert!(err.contains(&backup.display().to_string()) && err.contains(&dest.display().to_string()), "{err}");
        assert!(err.contains("is the recorded version") && err.contains("sync again"), "{case}: {err}");
        assert_eq!(fs::read(backup.join("SKILL.md")).unwrap(), fs::read(recorded.join("SKILL.md")).unwrap(), "{case}");
        assert!(fs::symlink_metadata(&dest).is_ok(), "{case}: left alone");
        // An update starting on top of it refuses the same way.
        let err = repo.update(Some("pdf"), true).unwrap_err().to_string();
        assert!(err.contains("is the recorded version"), "{case}: {err}");
        assert!(backup.is_dir(), "{case}");
    }

    // Neither matches the lock: still both kept.
    restore(&user);
    fs::write(backup.join("SKILL.md"), "edited").unwrap();
    let err = repo.recover_imports().unwrap_err().to_string();
    assert!(err.contains("is not what sk1ll.lock records") && backup.is_dir(), "{err}");

    // The replacement completed and was recorded: only then does the
    // backup go.
    restore(&|d: &Path| copy_dir(&recorded, d));
    fs::write(backup.join("SKILL.md"), "the old version").unwrap();
    assert!(repo.recover_imports().unwrap().is_empty());
    assert!(!backup.exists() && dest.join("SKILL.md").is_file());

    // Nothing installed: the backup is put back.
    restore(&|_: &Path| {});
    assert_eq!(repo.recover_imports().unwrap(), ["pdf"]);
    assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Clean);
}

#[test]
fn source_links_resolve_the_longest_ref_and_check_the_blob_at_that_commit() {
    let src = Source::new();
    let root = src.dir();
    git(&root, &["tag", "feature"]);
    git(&root, &["tag", "v1"]);
    src.release("skills/fake/SKILL.md/nested/SKILL.md", "Nested.\n");
    git(&root, &["tag", "-f", "v1"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let url = "https://github.com/fixture/source.git";
    let rewrite = format!("url.file://{}.insteadOf", root.display());
    let _git =
        failpoints::git_env(&[("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", &rewrite), ("GIT_CONFIG_VALUE_0", url)]);
    let repo = src.repo();
    let resolve = |tail: &str, path, git_ref| {
        repo.resolve_source(&format!("https://github.com/fixture/source{tail}"), path, git_ref)
    };
    for (link, path, git_ref, expected_ref, expected_path) in [
        ("/tree/feature/x/skills/pdf", None, None, "feature/x", "skills/pdf"),
        ("/tree/feature%2Fx/skills/pdf", None, Some("feature/x"), "feature/x", "skills/pdf"),
        ("/tree/feature/x/skills/web", Some("/skills/pdf/"), None, "feature/x", "skills/pdf"),
        ("/tree/main/skills/pdf", None, None, "main", "skills/pdf"),
        ("/blob/v1/skills/pdf/SKILL.md", None, None, "v1", "skills/pdf"),
        ("/tree/no-such-ref/missing", Some("skills/pdf"), Some("HEAD"), "HEAD", "skills/pdf"),
    ] {
        let got = resolve(link, path, git_ref).unwrap();
        assert_eq!(
            (got.source.as_str(), got.git_ref.as_str(), paths(&got)),
            ("github:fixture/source", expected_ref, vec![expected_path])
        );
    }
    let got = resolve(&format!("/tree/{head}/skills/pdf"), None, None).unwrap();
    assert_eq!(got.commit, head);
    assert_eq!(resolve("", None, None).unwrap().git_ref, "HEAD");
    for (link, git_ref, expected) in [
        ("/tree/feature/x/skills/pdf", Some("feature"), "conflicts with the ref in the link (`feature/x`)"),
        ("/tree/unknown/skills", None, "no branch, tag or commit"),
        ("/blob/v1/skills/fake/SKILL.md", None, "no `skills/fake/SKILL.md`"),
        ("/blob/v1/skills/SKILL.md", None, "no `skills/SKILL.md`"),
    ] {
        let error = resolve(link, None, git_ref).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn concurrent_resolution_of_one_source_returns_the_same_pinned_commit() {
    let src = Source::new();
    let root = src.dir();
    let expected = git(&root, &["rev-parse", "HEAD"]);
    let racers: Vec<_> = (0..6)
        .map(|_| {
            let repo = src.repo();
            let origin = src.origin();
            let dirs = src.s.home.dirs();
            std::thread::spawn(move || {
                let _dirs = failpoints::dirs(dirs);
                let _git = failpoints::git_env(sk1ll_testkit::git::ENV);
                repo.resolve_source(&origin, Some("skills/pdf"), Some("feature/x")).unwrap()
            })
        })
        .collect();
    for racer in racers {
        let result = racer.join().unwrap();
        assert_eq!(result.commit, expected);
        assert_eq!(paths(&result), ["skills/pdf"]);
    }
}

/// A repo about to add `pdf` (or, with `update`, to update it to [`V2`]),
/// with a draft skill `mine` next to it.
fn interruptible(update: bool) -> (Source, Repo) {
    let src = Source::new();
    let mut repo = src.repo();
    if update {
        src.add_pdf(&mut repo);
        src.release("skills/pdf/SKILL.md", V2);
    }
    write(&repo.personal_dir().join("mine/SKILL.md"), "my draft\n");
    (src, repo)
}

/// Adds or updates `pdf`; `fail_install` makes the install fail (EXDEV),
/// so an update rolls back.
fn run_import(src: &Source, repo: &mut Repo, update: bool, fail_install: bool) -> sk1ll_core::Result<()> {
    let _failure =
        fail_install.then(|| failpoints::fail_once(Point::SourceInstall, std::io::ErrorKind::CrossesDevices));
    if update { repo.update(Some("pdf"), false).map(drop) } else { src.try_add_pdf(repo).map(drop) }
}

/// Interrupted at every failpoint of an add, an update, or an update whose
/// install fails and rolls back: the draft is kept, no staging is left,
/// `imported/pdf` holds one whole version, and the manifest and lockfile
/// are as they were. Until the new tree is published, that version is the
/// previous one and the operation can simply be run again. After it is
/// published, see the known gap below.
#[test]
fn an_import_interrupted_at_any_failpoint_keeps_the_draft_and_one_whole_version() {
    for (update, fail_install) in [(false, false), (true, false), (true, true)] {
        let hits = {
            let (src, mut repo) = interruptible(update);
            let record = failpoints::record();
            assert_eq!(run_import(&src, &mut repo, update, fail_install).is_err(), fail_install);
            record.hits()
        };
        assert!(hits.iter().any(|hit| hit.point == Point::SourceInstall));
        assert_eq!(hits.iter().any(|hit| hit.point == Point::SourceRestore), fail_install);
        for (n, expected) in hits.iter().enumerate() {
            let case = format!("update={update}, fail_install={fail_install}, hit {n} ({:?})", expected.point);
            let (src, mut repo) = interruptible(update);
            let dest = repo.imported_dir().join("pdf/SKILL.md");
            let before = fs::read(&dest).ok();
            let (lock, manifest) = (repo.lock.skills.clone(), fs::read(repo.root().join("sk1ll.toml")).unwrap());
            let crash = {
                let _at = failpoints::crash_at(n);
                failpoints::catch(|| run_import(&src, &mut repo, update, fail_install)).unwrap_err()
            };
            assert_eq!(crash.point, expected.point, "{case}");
            let mut repo = src.repo();
            repo.recover_imports().unwrap();
            assert_eq!(fs::read_to_string(repo.personal_dir().join("mine/SKILL.md")).unwrap(), "my draft\n");
            assert_eq!(stages(&repo), 0, "{case}");
            assert_eq!(repo.lock.skills, lock, "{case}");
            assert_eq!(fs::read(repo.root().join("sk1ll.toml")).unwrap(), manifest, "{case}");
            let upstream = fs::read(src.dir().join("skills/pdf/SKILL.md")).unwrap();
            let published = !fail_install && hits[..n].iter().any(|hit| hit.point == Point::SourceInstall);
            if published {
                assert_eq!(fs::read(&dest).unwrap(), upstream, "{case}");
                continue; // pinned by known_gap_an_import_interrupted_after_publication_...
            }
            assert_eq!(fs::read(&dest).ok(), before, "{case}");
            run_import(&src, &mut repo, update, false).unwrap();
            assert_eq!(fs::read(&dest).unwrap(), upstream, "{case}");
            assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Clean, "{case}");
        }
    }
}

/// KNOWN GAP, pinned as it is (not as it should be): between publishing
/// the new tree at `imported/pdf` (the rename after `SourceInstall`) and
/// writing `sk1ll.lock` (then, for an add, `sk1ll.toml`), a crash leaves
/// the whole new tree with stale or missing metadata, and
/// `recover_imports` doesn't repair it. Nothing is lost, but:
/// - an interrupted add leaves `imported/pdf` recorded nowhere; it shows in
///   the Draft as a new skill, and adding it again is refused ("not a
///   recorded import") until the user moves it away by hand;
/// - an interrupted update leaves the new tree under the old lock entry, so
///   it reads as locally modified: `update` refuses and only a forced
///   update records it.
///
/// The only failpoint in that window is the `HashMidRead` of hashing the
/// published tree for the lock. When this gap is fixed, turn this test
/// around to assert the recovered state.
#[test]
fn known_gap_an_import_interrupted_after_publication_is_left_with_stale_or_missing_metadata() {
    for update in [false, true] {
        let (src, mut repo) = interruptible(update);
        let (lock, manifest) = (fs::read(repo.root().join("sk1ll.lock")).ok(), repo.manifest.import.clone());
        let mut published = false;
        let crash = failpoints::scoped(
            move |point, _| match point {
                Point::SourceInstall => {
                    published = true;
                    Action::Continue
                }
                _ if published => Action::Crash,
                _ => Action::Continue,
            },
            || failpoints::catch(|| run_import(&src, &mut repo, update, false)),
        )
        .unwrap_err();
        assert_eq!(crash.point, Point::HashMidRead, "update={update}");

        let mut repo = src.repo();
        assert!(repo.recover_imports().unwrap().is_empty(), "nothing for recovery to do");
        let upstream = fs::read_to_string(src.dir().join("skills/pdf/SKILL.md")).unwrap();
        assert_eq!(installed(&repo), upstream, "the new tree is published, whole");
        assert_eq!(fs::read(repo.root().join("sk1ll.lock")).ok(), lock, "but the lockfile never recorded it");
        assert_eq!(repo.manifest.import, manifest);
        assert_eq!(stages(&repo), 0);
        if update {
            assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Modified);
            let error = repo.update(Some("pdf"), false).unwrap_err().to_string();
            assert!(error.contains("locally modified imports: pdf"), "{error}");
            repo.update(Some("pdf"), true).unwrap();
        } else {
            assert!(repo.import_status().unwrap().is_empty(), "recorded nowhere");
            assert!(repo.draft().unwrap().iter().any(|d| d.item == sk1ll_core::ItemRef::Skill("pdf".into())));
            let error = src.try_add_pdf(&mut repo).unwrap_err().to_string();
            assert!(error.contains("not a recorded import"), "{error}");
            fs::rename(repo.imported_dir().join("pdf"), src.s.join("moved-away")).unwrap();
            src.add_pdf(&mut repo);
        }
        assert_eq!(installed(&repo), upstream);
        assert_eq!(repo.import_status().unwrap()[0].state, ImportState::Clean);
    }
}
