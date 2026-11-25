//! Git plumbing beyond [`crate::Repo`]: reading refs, ancestry, unpushed
//! items, and setting up the local clone of a fleet's repo. Never prompts.
//!
//! Every command, clone included, runs through [`super::Git`], so it
//! has the same execution deadline as core's own git calls
//! (`SK1LL_GIT_TIMEOUT` / `SK1LL_GIT_LOCAL_TIMEOUT`): a server that accepts the
//! connection and then never answers can't hold the daemon forever.
//!
//! No URL here carries credentials (git authenticates through its
//! environment, see [`super::config_env`]), and error messages are redacted
//! all the same ([`crate::redact`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use super::{Git, RepoGit};
use crate::draft::{FLEET_BRANCH, REMOTE};
use crate::redact::redact;
use crate::{Error, IoExt, ItemRef, Repo, Result};
use serde::{Deserialize, Serialize};

type Env<'a> = &'a [(String, String)];

/// Every command here runs on the skills repo (or its clone being made),
/// so it is built by [`RepoGit`]: with each filter driver git's
/// configuration for `dir` names turned off.
fn command(dir: &Path, args: &[&str], env: Env) -> Result<Git> {
    Ok(RepoGit::with_env(dir, env).map_err(redacted)?.git().env("GIT_TERMINAL_PROMPT", "0").args(args))
}

/// A core error with the URL-ish parts redacted: a git one's arguments and
/// output, any other's message (kept as it is when there is nothing to
/// redact in it).
fn redacted(e: Error) -> Error {
    match e {
        Error::Git { args, stderr } => Error::Git { args: redact(&args), stderr: redact(&stderr) },
        e => {
            let message = e.to_string();
            let clean = redact(&message);
            if clean == message { e } else { Error::Invalid(clean) }
        }
    }
}

fn output(dir: &Path, args: &[&str], env: Env) -> Result<Output> {
    command(dir, args, env)?.output().map_err(redacted)
}

fn failed(args: &[&str], out: &Output) -> Error {
    Error::Git { args: redact(&args.join(" ")), stderr: redact(String::from_utf8_lossy(&out.stderr).trim()) }
}

/// Diff two text snapshots using Git's unified diff, without touching the repo.
pub fn diff_text(before: Option<&str>, after: &str) -> Result<String> {
    if before == Some(after) {
        return Ok(String::new());
    }
    let scratch = temp_dir()?;
    let old = if let Some(before) = before {
        let path = scratch.path().join("on-disk");
        std::fs::write(&path, before).at(path)?;
        "on-disk"
    } else {
        "/dev/null"
    };
    let next = scratch.path().join("next-sync");
    std::fs::write(&next, after).at(next)?;
    let args = ["diff", "--no-index", "--no-color", "--no-ext-diff", "--no-textconv", "--text", "--", old, "next-sync"];
    let out = output(scratch.path(), &args, &[])?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(failed(&args, &out));
    }
    String::from_utf8(out.stdout).map_err(|_| Error::Invalid("`git diff` printed text that is not UTF-8".into()))
}

/// A scratch directory in the system's temporary directory.
fn temp_dir() -> Result<tempfile::TempDir> {
    tempfile::tempdir().at(std::env::temp_dir())
}

/// `git diff --no-index` of two paths outside any repo, without external
/// diff drivers or textconv: exit status 1 means they differ.
pub fn diff_no_index(left: &Path, right: &Path) -> std::io::Result<Output> {
    std::process::Command::new("git")
        .args(["diff", "--no-index", "--no-ext-diff", "--no-textconv", "--"])
        .arg(left)
        .arg(right)
        .output()
}

/// Runs git; non-zero is an error. Returns trimmed stdout.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    run_env(dir, args, &[])
}

fn run_env(dir: &Path, args: &[&str], env: Env) -> Result<String> {
    let out = output(dir, args, env)?;
    if !out.status.success() {
        return Err(failed(args, &out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs git; `None` on a non-zero exit.
fn try_run(dir: &Path, args: &[&str]) -> Result<Option<String>> {
    let out = output(dir, args, &[])?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

pub fn rev(dir: &Path, name: &str) -> Result<Option<String>> {
    try_run(dir, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")])
}

pub fn branch(dir: &Path) -> Result<Option<String>> {
    try_run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

/// `refs/remotes/origin/main`: the server's baseline as last fetched or pushed.
pub fn remote_head(dir: &Path) -> Result<Option<String>> {
    rev(dir, &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))
}

/// Fetches every branch of origin (pruning the ones it no longer has), so
/// `refs/remotes/origin/*` is exactly what the server has.
fn fetch_all(dir: &Path, env: Env) -> Result<()> {
    let refspec = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
    run_env(dir, &["fetch", "--quiet", "--prune", REMOTE, &refspec], env).map(drop)
}

fn no_fleet_branch(others: &[String]) -> Error {
    if others.is_empty() {
        Error::Invalid(format!("the server has no commits on `{FLEET_BRANCH}`"))
    } else {
        Error::Invalid(format!("the server has no `{FLEET_BRANCH}` branch (it has {})", others.join(", ")))
    }
}

/// The branches origin had at the last [`fetch_all`].
fn remote_branches(dir: &Path) -> Result<Vec<String>> {
    let prefix = format!("refs/remotes/{REMOTE}/");
    let refs = run(dir, &["for-each-ref", "--format=%(refname)", &prefix])?;
    Ok(refs.lines().filter_map(|r| r.strip_prefix(&prefix)).filter(|b| *b != "HEAD").map(str::to_string).collect())
}

pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> Result<bool> {
    Ok(output(dir, &["merge-base", "--is-ancestor", a, b], &[])?.status.success())
}

/// The server baseline the working tree is built on: origin's branch if `HEAD`
/// contains it, else where `HEAD` forked from it (Blocked: the old baseline).
pub fn baseline(dir: &Path) -> Result<Option<String>> {
    let (Some(remote), Some(head)) = (remote_head(dir)?, rev(dir, "HEAD")?) else {
        return Ok(None);
    };
    if is_ancestor(dir, &remote, &head)? {
        return Ok(Some(remote));
    }
    try_run(dir, &["merge-base", &head, &remote])
}

/// Whether `HEAD` has commits origin doesn't.
pub fn has_unpushed(dir: &Path) -> Result<bool> {
    let Some(head) = rev(dir, "HEAD")? else {
        return Ok(false);
    };
    match remote_head(dir)? {
        Some(remote) => Ok(!is_ancestor(dir, &head, &remote)?),
        None => Ok(true),
    }
}

/// Items touched by commits in `HEAD` that origin doesn't have.
pub fn unpushed_items(dir: &Path) -> Result<Vec<ItemRef>> {
    if !has_unpushed(dir)? {
        return Ok(Vec::new());
    }
    let range = match remote_head(dir)? {
        Some(remote) => format!("{remote}..HEAD"),
        None => "HEAD".to_string(),
    };
    let args = ["log", "--format=", "--name-only", "--no-renames", "-z", &range, "--"];
    let out = output(dir, &args, &[])?;
    if !out.status.success() {
        return Err(failed(&args, &out));
    }
    let items: BTreeSet<ItemRef> = out
        .stdout
        .split(|b| *b == 0)
        .map(|p| String::from_utf8_lossy(p).trim_matches('\n').to_string())
        .filter(|p| !p.is_empty())
        .map(|p| ItemRef::for_path(Path::new(&p)))
        .collect();
    Ok(items.into_iter().collect())
}

/// The first baseline can clash with files even though HEAD is unborn.
pub enum FirstBaseline {
    Empty,
    Adopted(String),
    Blocked { baseline: String, items: Vec<ItemRef> },
}

/// Adopts an empty clone's first baseline, keeping the generated init files
/// as draft. Every checkout destination is checked before HEAD or the index
/// moves. Ignored files and symlinks are checked directly on disk too.
/// Talks to no server: the caller has fetched origin's branches.
pub fn adopt_first_baseline(dir: &Path) -> Result<FirstBaseline> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let Some(remote) = remote_head(dir)? else {
        let others = remote_branches(dir)?;
        if others.is_empty() {
            return Ok(FirstBaseline::Empty);
        }
        return Err(no_fleet_branch(&others));
    };
    let args = ["ls-tree", "-rz", &remote];
    let tree = output(dir, &args, &[])?;
    if !tree.status.success() {
        return Err(failed(&args, &tree));
    }
    // An empty server clone and /init already contain core's starter files.
    // Keep those untouched; any other existing target belongs to the user.
    let template = temp_dir()?;
    Repo::init(template.path())?;
    let mut missing = Vec::new();
    let mut blocked = BTreeSet::new();
    for entry in tree.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let tab =
            entry.iter().position(|b| *b == b'\t').ok_or_else(|| Error::Invalid("invalid ls-tree entry".into()))?;
        let fields = std::str::from_utf8(&entry[..tab])
            .map_err(|_| Error::Invalid("invalid ls-tree entry".into()))?
            .split_whitespace()
            .collect::<Vec<_>>();
        let path = Path::new(OsStr::from_bytes(&entry[tab + 1..]));
        let mut destination = dir.to_path_buf();
        let mut obstructed = false;
        for component in path.components() {
            destination.push(component);
            match destination.symlink_metadata() {
                Ok(meta) if destination != dir.join(path) => {
                    if !meta.is_dir() {
                        obstructed = true;
                        break;
                    }
                }
                Ok(meta) => {
                    let starter = template.path().join(path);
                    if !meta.is_file()
                        || !starter.is_file()
                        || std::fs::read(&destination).at(&destination)? != std::fs::read(&starter).at(&starter)?
                    {
                        obstructed = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).at(destination),
            }
        }
        if obstructed {
            blocked.insert(ItemRef::for_path(path));
        } else if destination.symlink_metadata().is_err() {
            missing.push((path.to_path_buf(), fields[0].to_string(), fields[2].to_string()));
        }
    }
    if !blocked.is_empty() {
        return Ok(FirstBaseline::Blocked { baseline: remote, items: blocked.into_iter().collect() });
    }
    crate::failpoint!(BaselinePublish, dir);
    let directory = crate::placement::Directory::open(dir, false).at(dir)?;
    for (path, mode, oid) in missing {
        let blob = output(dir, &["cat-file", "blob", &oid], &[])?;
        if !blob.status.success() {
            return Err(failed(&["cat-file", "blob", &oid], &blob));
        }
        let published = match mode.as_str() {
            "100644" | "100755" => directory.create_file(&path, &blob.stdout, mode == "100755"),
            "120000" => directory.create_symlink(&path, OsStr::from_bytes(&blob.stdout)),
            _ => Err(std::io::Error::other("unsupported baseline file mode")),
        };
        if published.is_err() {
            blocked.insert(ItemRef::for_path(&path));
        }
    }
    if !blocked.is_empty() {
        // Keep earlier, exclusively published files. Removing them could lose
        // an edit made after publication. HEAD and index remain unborn.
        return Ok(FirstBaseline::Blocked { baseline: remote, items: blocked.into_iter().collect() });
    }
    // Git only writes metadata, after every working-tree publication succeeds.
    run(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{FLEET_BRANCH}")])?;
    run_env(dir, &["reset", "--quiet", "--mixed", &remote], &[])?;
    Ok(FirstBaseline::Adopted(remote))
}

/// A repo holding only the commit `init_committed` makes takes a first
/// baseline of unrelated history as an unborn init would: that commit is
/// dropped, so the starter files are draft again, and the baseline adopted.
/// Unless it is adopted, the commit is restored. `None` when this doesn't apply.
/// Like [`adopt_first_baseline`], it reads what the caller fetched.
pub fn adopt_first_baseline_over_init(dir: &Path) -> Result<Option<FirstBaseline>> {
    let Some(head) = rev(dir, "HEAD")? else { return Ok(None) };
    if branch(dir)?.as_deref() != Some(FLEET_BRANCH) || rev(dir, "HEAD^")?.is_some() {
        return Ok(None);
    }
    let template = temp_dir()?;
    Repo::init(template.path())?;
    run(template.path(), &["add", "--all"])?;
    if run(template.path(), &["write-tree"])? != run(dir, &["rev-parse", "HEAD^{tree}"])? {
        return Ok(None);
    }
    match remote_head(dir)? {
        Some(remote) if try_run(dir, &["merge-base", &head, &remote])?.is_none() => {}
        _ => return Ok(None),
    }
    let branch = format!("refs/heads/{FLEET_BRANCH}");
    run(dir, &["update-ref", "-d", &branch, &head])?;
    let adopted = adopt_first_baseline(dir);
    if !matches!(adopted, Ok(FirstBaseline::Adopted(_))) {
        // Files published before a block stay as draft, as for an unborn init.
        run(dir, &["update-ref", &branch, &head, ""])?;
    }
    adopted.map(Some)
}

/// What [`ensure_clone`] found at the repo path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// Cloned the server's repo.
    Cloned,
    /// An existing clone of the server's repo; its origin was updated.
    Adopted,
}

/// Whether `dir` can become the machine's clone of `remote`: missing, an
/// empty directory, or a git repo whose origin is `remote` (credentials
/// aside). Checked before joining, so a refusal costs no join code.
pub fn check_repo_path(dir: &Path, remote: &str) -> Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).at(dir),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return Ok(());
            }
        }
    }
    if !dir.join(".git").exists() {
        return Err(Error::Invalid(format!(
            "{} exists and is not empty or a clone of the server's repo; pick another --repo",
            dir.display()
        )));
    }
    let origin = try_run(dir, &["remote", "get-url", REMOTE])?;
    match origin {
        Some(url) if without_credentials(&url) == without_credentials(remote) => check_fleet_branch(dir),
        Some(url) => Err(Error::Invalid(format!(
            "{} is a clone of {}, not of the server ({}); pick another --repo",
            dir.display(),
            without_credentials(&url),
            without_credentials(remote)
        ))),
        None => {
            Err(Error::Invalid(format!("{} is a git repo without an `{REMOTE}`; pick another --repo", dir.display())))
        }
    }
}

/// An existing clone must be on the fleet branch: the server's baseline is
/// `main`, and a clone on another branch would pull nothing and push a
/// separate branch the fleet never sees. An unborn `HEAD` is fine (it is
/// pointed at `main` before anything is committed).
fn check_fleet_branch(dir: &Path) -> Result<()> {
    if rev(dir, "HEAD")?.is_none() {
        return Ok(());
    }
    match branch(dir)? {
        Some(b) if b == FLEET_BRANCH => Ok(()),
        Some(b) => Err(Error::Invalid(format!(
            "{} is on branch `{b}`, but the fleet's baseline is `{FLEET_BRANCH}`; rename it (`git -C {} branch -m {b} \
             {FLEET_BRANCH}`) or check out `{FLEET_BRANCH}`, or pick another --repo",
            dir.display(),
            dir.display()
        ))),
        None => Err(Error::Invalid(format!(
            "{} has a detached HEAD; check out `{FLEET_BRANCH}` (the fleet's baseline) or pick another --repo",
            dir.display()
        ))),
    }
}

/// For an existing clone being adopted at join: fetches the server and checks
/// that the clone's history and the server's `main` share a commit, so the
/// first pull or push can't go off on a history of its own. A clone without
/// commits has no local history to check.
/// `env` authenticates git, as for [`ensure_clone`].
pub fn check_shares_history(dir: &Path, env: &[(String, String)]) -> Result<()> {
    check_fleet_branch(dir)?;
    fetch_all(dir, env)?;
    let Some(head) = rev(dir, "HEAD")? else {
        return Ok(());
    };
    let Some(remote) = remote_head(dir)? else {
        let others = remote_branches(dir)?;
        return Err(no_fleet_branch(&others));
    };
    if output(dir, &["merge-base", &head, &remote], &[])?.status.success() {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "{}'s `{FLEET_BRANCH}` shares no history with the server's `{FLEET_BRANCH}` (it was started on its own, not \
         cloned from the server), so its commits can't be pulled or pushed; move it aside and the daemon clones the server's \
         repo there, or pick another --repo",
        dir.display()
    )))
}

/// Makes `dir` a clone of `remote` (see [`check_repo_path`]). `env` is how
/// git authenticates (see [`super::config_env`]); origin
/// never carries credentials, and an existing clone whose origin (or push
/// URL) still has them from an older version is rewritten without them.
pub fn ensure_clone(dir: &Path, remote: &str, env: &[(String, String)]) -> Result<Setup> {
    let remote = without_credentials(remote);
    check_repo_path(dir, &remote)?;
    let setup = if dir.join(".git").exists() {
        run(dir, &["remote", "set-url", REMOTE, &remote])?;
        let push_urls = try_run(dir, &["config", "--get-all", &format!("remote.{REMOTE}.pushurl")])?;
        if push_urls.is_some_and(|urls| urls.lines().any(has_credentials)) {
            run(dir, &["config", "--unset-all", &format!("remote.{REMOTE}.pushurl")])?;
        }
        Setup::Adopted
    } else {
        // Cloned beside `dir` and moved into place once complete, so nothing
        // that runs meanwhile (the daemon clones outside its engine mutex)
        // takes a half-made clone for the repo. `dir` is missing or empty.
        let parent: PathBuf = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into());
        std::fs::create_dir_all(&parent).at(&parent)?;
        let stage = tempfile::Builder::new().prefix(".sk1ll-clone-").tempdir_in(&parent).at(&parent)?;
        let target = stage.path().join("repo");
        let clone = ["clone", "--quiet", "--no-checkout", "--origin", REMOTE, &remote, &target.to_string_lossy()];
        run_env(stage.path(), &clone, env)?;
        check_out_clone(&target, env)?;
        std::fs::rename(&target, dir).at(dir)?;
        Setup::Cloned
    };
    private_config(dir)?;
    Ok(setup)
}

/// Checks out a clone made with `--no-checkout`. Its filter drivers are
/// read from the clone itself, where git's configuration (a conditional
/// include on its `gitdir`, say) applies as it does for the checkout, and
/// turned off for it; a Baseline that writes a file through one is refused,
/// as a pull refuses it (see [`crate::git::driver_paths`]).
fn check_out_clone(dir: &Path, env: Env) -> Result<()> {
    if rev(dir, "HEAD")?.is_none() {
        return Ok(());
    }
    let repo = RepoGit::with_env(dir, env).map_err(redacted)?;
    let listed = repo.git().args(["ls-tree", "-r", "-z", "--name-only", "HEAD"]).run().map_err(redacted)?;
    let paths: Vec<PathBuf> = crate::git::split_paths(&listed).collect();
    let paths: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let filtered = crate::git::driver_paths(repo.git(), Some("HEAD"), &paths, repo.drivers()).map_err(redacted)?;
    if !filtered.is_empty() {
        let names: Vec<String> = filtered.iter().map(|p| p.display().to_string()).collect();
        return Err(Error::Invalid(format!(
            "the server's Baseline writes {} through a git filter driver (`filter` in .gitattributes), which the \
             skills repo doesn't support; remove it from .gitattributes there",
            names.join(", ")
        )));
    }
    repo.git().args(["reset", "--quiet", "--hard", "HEAD"]).run().map_err(redacted)?;
    Ok(())
}

fn private_config(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(".git").join("config");
    if path.is_file() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).at(&path)?;
    }
    Ok(())
}

/// Whether this repo defines its own origin remote.
pub fn has_origin(dir: &Path) -> Result<bool> {
    Ok(try_run(dir, &["remote", "get-url", "origin"])?.is_some())
}

/// A standalone repo may join only when it is an untouched init, or an
/// unchanged clone of this server without any local commits to preserve.
pub fn check_standalone_join(dir: &Path, remote: &str) -> Result<()> {
    if is_pristine_init(dir)? {
        return Ok(());
    }
    // Git status hides ignored content. An unborn repo that is not the
    // complete pristine init may only be adopted when it has no worktree entries.
    if dir.join(".git").exists()
        && rev(dir, "HEAD")?.is_none()
        && std::fs::read_dir(dir).at(dir)?.any(|entry| entry.map_or(true, |e| e.file_name() != ".git"))
    {
        return Err(Error::Invalid(
            "the standalone repo has a draft or local commits, or other content outside a pristine init; move it aside before joining a Fleet"
                .into(),
        ));
    }
    if dir.join(".git").exists()
        && (!run(dir, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() || has_unpushed(dir)?)
    {
        return Err(Error::Invalid(
            "the standalone repo has a draft or local commits; move it aside or choose another repo before joining a Fleet"
                .into(),
        ));
    }
    check_repo_path(dir, remote)?;
    Ok(())
}

/// Compare every non-git entry with core's init output, including ignored
/// files and symlinks. No user content is removed when this directory moves.
/// The repo may be unborn, or hold only the commit of those files that
/// `init_committed` makes: one root commit on the fleet branch, no other
/// refs, nothing staged or changed.
pub fn is_pristine_init(dir: &Path) -> Result<bool> {
    if dir.symlink_metadata().is_ok_and(|m| m.is_symlink()) || !dir.join(".git").is_dir() {
        return Ok(false);
    }
    if rev(dir, "HEAD")?.is_some()
        && (rev(dir, "HEAD^")?.is_some()
            || run(dir, &["for-each-ref", "--format=%(refname)"])? != format!("refs/heads/{FLEET_BRANCH}")
            || !run(dir, &["status", "--porcelain", "--untracked-files=all", "--ignored"])?.is_empty())
    {
        return Ok(false);
    }
    fn files(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>) -> Result<bool> {
        for entry in std::fs::read_dir(dir).at(dir)? {
            let path = entry.at(dir)?.path();
            let rel = path.strip_prefix(root).expect("a directory's entry is inside its root");
            if rel == Path::new(".git") {
                continue;
            }
            let meta = path.symlink_metadata().at(&path)?;
            if meta.is_symlink() {
                return Ok(false);
            }
            if meta.is_dir() {
                out.insert(rel.to_path_buf(), None);
                if !files(root, &path, out)? {
                    return Ok(false);
                }
            } else if meta.is_file() {
                out.insert(rel.to_path_buf(), Some(std::fs::read(&path).at(&path)?));
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let template = temp_dir()?;
    Repo::init(template.path())?;
    let (mut expected, mut actual) = (Default::default(), Default::default());
    Ok(files(template.path(), template.path(), &mut expected)? && files(dir, dir, &mut actual)? && expected == actual)
}

/// `url` without `user:password@`, for comparing remotes.
pub fn without_credentials(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            let path = path.trim_end_matches('/');
            if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
        }
        None => url.trim_end_matches('/').to_string(),
    }
}

/// Whether `url` carries `user[:password]@`.
pub fn has_credentials(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, rest)| rest.split('/').next().is_some_and(|host| host.contains('@')))
}

/// The canonical git directory of the repo at `root`, `None` while it is no
/// git repo (yet).
pub fn absolute_git_dir(root: &Path) -> Option<PathBuf> {
    let out = RepoGit::open(root)
        .ok()?
        .git()
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    Path::new(text.trim_end_matches('\n')).canonicalize().ok()
}
