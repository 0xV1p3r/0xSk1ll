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
use crate::{ItemRef, Repo};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

type Env<'a> = &'a [(String, String)];

/// Every command here runs on the skills repo (or its clone being made),
/// so it is built by [`RepoGit`]: with each filter driver git's
/// configuration for `dir` names turned off.
fn command(dir: &Path, args: &[&str], env: Env) -> anyhow::Result<Git> {
    Ok(RepoGit::with_env(dir, env).map_err(redacted)?.git().env("GIT_TERMINAL_PROMPT", "0").args(args))
}

/// A core git error with the URL-ish parts redacted.
fn redacted(e: crate::Error) -> anyhow::Error {
    match e {
        crate::Error::Git { args, stderr } => {
            anyhow::anyhow!("`git {}` failed: {}", redact(&args), redact(&stderr))
        }
        e => anyhow::anyhow!("{}", redact(&e.to_string())),
    }
}

fn output(dir: &Path, args: &[&str], env: Env) -> anyhow::Result<Output> {
    command(dir, args, env)?.output().map_err(redacted)
}

fn failed(args: &[&str], out: &Output) -> anyhow::Error {
    anyhow::anyhow!("`git {}` failed: {}", redact(&args.join(" ")), redact(String::from_utf8_lossy(&out.stderr).trim()))
}

/// Diff two text snapshots using Git's unified diff, without touching the repo.
pub fn diff_text(before: Option<&str>, after: &str) -> anyhow::Result<String> {
    if before == Some(after) {
        return Ok(String::new());
    }
    let scratch = tempfile::tempdir()?;
    let old = if let Some(before) = before {
        std::fs::write(scratch.path().join("on-disk"), before)?;
        "on-disk"
    } else {
        "/dev/null"
    };
    std::fs::write(scratch.path().join("next-sync"), after)?;
    let args = ["diff", "--no-index", "--no-color", "--no-ext-diff", "--no-textconv", "--text", "--", old, "next-sync"];
    let out = output(scratch.path(), &args, &[])?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(failed(&args, &out));
    }
    Ok(String::from_utf8(out.stdout)?)
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
pub fn run(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    run_env(dir, args, &[])
}

fn run_env(dir: &Path, args: &[&str], env: Env) -> anyhow::Result<String> {
    let out = output(dir, args, env)?;
    if !out.status.success() {
        return Err(failed(args, &out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs git; `None` on a non-zero exit.
fn try_run(dir: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let out = output(dir, args, &[])?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

pub fn rev(dir: &Path, name: &str) -> anyhow::Result<Option<String>> {
    try_run(dir, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")])
}

pub fn branch(dir: &Path) -> anyhow::Result<Option<String>> {
    try_run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

/// `refs/remotes/origin/main`: the server's baseline as last fetched or pushed.
pub fn remote_head(dir: &Path) -> anyhow::Result<Option<String>> {
    rev(dir, &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))
}

/// Fetches every branch of origin (pruning the ones it no longer has), so
/// `refs/remotes/origin/*` is exactly what the server has.
fn fetch_all(dir: &Path, env: Env) -> anyhow::Result<()> {
    let refspec = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
    run_env(dir, &["fetch", "--quiet", "--prune", REMOTE, &refspec], env).map(drop)
}

fn no_fleet_branch(others: &[String]) -> anyhow::Error {
    if others.is_empty() {
        anyhow::anyhow!("the server has no commits on `{FLEET_BRANCH}`")
    } else {
        anyhow::anyhow!("the server has no `{FLEET_BRANCH}` branch (it has {})", others.join(", "))
    }
}

/// The branches origin had at the last [`fetch_all`].
fn remote_branches(dir: &Path) -> anyhow::Result<Vec<String>> {
    let prefix = format!("refs/remotes/{REMOTE}/");
    let refs = run(dir, &["for-each-ref", "--format=%(refname)", &prefix])?;
    Ok(refs.lines().filter_map(|r| r.strip_prefix(&prefix)).filter(|b| *b != "HEAD").map(str::to_string).collect())
}

pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> anyhow::Result<bool> {
    Ok(output(dir, &["merge-base", "--is-ancestor", a, b], &[])?.status.success())
}

/// The server baseline the working tree is built on: origin's branch if `HEAD`
/// contains it, else where `HEAD` forked from it (Blocked: the old baseline).
pub fn baseline(dir: &Path) -> anyhow::Result<Option<String>> {
    let (Some(remote), Some(head)) = (remote_head(dir)?, rev(dir, "HEAD")?) else {
        return Ok(None);
    };
    if is_ancestor(dir, &remote, &head)? {
        return Ok(Some(remote));
    }
    try_run(dir, &["merge-base", &head, &remote])
}

/// Whether `HEAD` has commits origin doesn't.
pub fn has_unpushed(dir: &Path) -> anyhow::Result<bool> {
    let Some(head) = rev(dir, "HEAD")? else {
        return Ok(false);
    };
    match remote_head(dir)? {
        Some(remote) => Ok(!is_ancestor(dir, &head, &remote)?),
        None => Ok(true),
    }
}

/// Items touched by commits in `HEAD` that origin doesn't have.
pub fn unpushed_items(dir: &Path) -> anyhow::Result<Vec<ItemRef>> {
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

#[cfg(test)]
thread_local! {
    static BEFORE_BASELINE_PUBLICATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

/// Adopts an empty clone's first baseline, keeping the generated init files
/// as draft. Every checkout destination is checked before HEAD or the index
/// moves. Ignored files and symlinks are checked directly on disk too.
/// Talks to no server: the caller has fetched origin's branches.
pub fn adopt_first_baseline(dir: &Path) -> anyhow::Result<FirstBaseline> {
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
    let template = tempfile::tempdir()?;
    Repo::init(template.path())?;
    let mut missing = Vec::new();
    let mut blocked = BTreeSet::new();
    for entry in tree.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let tab = entry.iter().position(|b| *b == b'\t').context("invalid ls-tree entry")?;
        let fields = std::str::from_utf8(&entry[..tab])?.split_whitespace().collect::<Vec<_>>();
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
                    if !meta.is_file() || !starter.is_file() || std::fs::read(&destination)? != std::fs::read(starter)?
                    {
                        obstructed = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("checking {}", destination.display())),
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
    #[cfg(test)]
    BEFORE_BASELINE_PUBLICATION.with_borrow_mut(|hook| {
        if let Some(hook) = hook.take() {
            hook();
        }
    });
    let directory = crate::placement::Directory::open(dir, false)?;
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
pub fn adopt_first_baseline_over_init(dir: &Path) -> anyhow::Result<Option<FirstBaseline>> {
    let Some(head) = rev(dir, "HEAD")? else { return Ok(None) };
    if branch(dir)?.as_deref() != Some(FLEET_BRANCH) || rev(dir, "HEAD^")?.is_some() {
        return Ok(None);
    }
    let template = tempfile::tempdir()?;
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
pub fn check_repo_path(dir: &Path, remote: &str) -> anyhow::Result<()> {
    match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return Ok(());
            }
        }
    }
    if !dir.join(".git").exists() {
        bail!("{} exists and is not empty or a clone of the server's repo; pick another --repo", dir.display());
    }
    let origin = try_run(dir, &["remote", "get-url", REMOTE])?;
    match origin {
        Some(url) if without_credentials(&url) == without_credentials(remote) => check_fleet_branch(dir),
        Some(url) => bail!(
            "{} is a clone of {}, not of the server ({}); pick another --repo",
            dir.display(),
            without_credentials(&url),
            without_credentials(remote)
        ),
        None => bail!("{} is a git repo without an `{REMOTE}`; pick another --repo", dir.display()),
    }
}

/// An existing clone must be on the fleet branch: the server's baseline is
/// `main`, and a clone on another branch would pull nothing and push a
/// separate branch the fleet never sees. An unborn `HEAD` is fine (it is
/// pointed at `main` before anything is committed).
fn check_fleet_branch(dir: &Path) -> anyhow::Result<()> {
    if rev(dir, "HEAD")?.is_none() {
        return Ok(());
    }
    match branch(dir)? {
        Some(b) if b == FLEET_BRANCH => Ok(()),
        Some(b) => bail!(
            "{} is on branch `{b}`, but the fleet's baseline is `{FLEET_BRANCH}`; rename it (`git -C {} branch -m {b} \
             {FLEET_BRANCH}`) or check out `{FLEET_BRANCH}`, or pick another --repo",
            dir.display(),
            dir.display()
        ),
        None => bail!(
            "{} has a detached HEAD; check out `{FLEET_BRANCH}` (the fleet's baseline) or pick another --repo",
            dir.display()
        ),
    }
}

/// For an existing clone being adopted at join: fetches the server and checks
/// that the clone's history and the server's `main` share a commit, so the
/// first pull or push can't go off on a history of its own. A clone without
/// commits has no local history to check.
/// `env` authenticates git, as for [`ensure_clone`].
pub fn check_shares_history(dir: &Path, env: &[(String, String)]) -> anyhow::Result<()> {
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
    bail!(
        "{}'s `{FLEET_BRANCH}` shares no history with the server's `{FLEET_BRANCH}` (it was started on its own, not \
         cloned from the server), so its commits can't be pulled or pushed; move it aside and the daemon clones the server's \
         repo there, or pick another --repo",
        dir.display()
    )
}

/// Makes `dir` a clone of `remote` (see [`check_repo_path`]). `env` is how
/// git authenticates (see [`super::config_env`]); origin
/// never carries credentials, and an existing clone whose origin (or push
/// URL) still has them from an older version is rewritten without them.
pub fn ensure_clone(dir: &Path, remote: &str, env: &[(String, String)]) -> anyhow::Result<Setup> {
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
        std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
        let stage = tempfile::Builder::new()
            .prefix(".sk1ll-clone-")
            .tempdir_in(&parent)
            .with_context(|| format!("creating a directory in {}", parent.display()))?;
        let target = stage.path().join("repo");
        let clone = ["clone", "--quiet", "--no-checkout", "--origin", REMOTE, &remote, &target.to_string_lossy()];
        run_env(stage.path(), &clone, env)?;
        check_out_clone(&target, env)?;
        std::fs::rename(&target, dir).with_context(|| format!("moving the clone to {}", dir.display()))?;
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
fn check_out_clone(dir: &Path, env: Env) -> anyhow::Result<()> {
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
        bail!(
            "the server's Baseline writes {} through a git filter driver (`filter` in .gitattributes), which the \
             skills repo doesn't support; remove it from .gitattributes there",
            names.join(", ")
        );
    }
    repo.git().args(["reset", "--quiet", "--hard", "HEAD"]).run().map_err(redacted)?;
    Ok(())
}

fn private_config(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(".git").join("config");
    if path.is_file() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    Ok(())
}

/// Whether this repo defines its own origin remote.
pub fn has_origin(dir: &Path) -> anyhow::Result<bool> {
    Ok(try_run(dir, &["remote", "get-url", "origin"])?.is_some())
}

/// A standalone repo may join only when it is an untouched init, or an
/// unchanged clone of this server without any local commits to preserve.
pub fn check_standalone_join(dir: &Path, remote: &str) -> anyhow::Result<()> {
    if is_pristine_init(dir)? {
        return Ok(());
    }
    // Git status hides ignored content. An unborn repo that is not the
    // complete pristine init may only be adopted when it has no worktree entries.
    if dir.join(".git").exists()
        && rev(dir, "HEAD")?.is_none()
        && std::fs::read_dir(dir)?.any(|entry| entry.map_or(true, |e| e.file_name() != ".git"))
    {
        bail!(
            "the standalone repo has a draft or local commits, or other content outside a pristine init; move it aside before joining a Fleet"
        );
    }
    if dir.join(".git").exists()
        && (!run(dir, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() || has_unpushed(dir)?)
    {
        bail!(
            "the standalone repo has a draft or local commits; move it aside or choose another repo before joining a Fleet"
        );
    }
    check_repo_path(dir, remote)?;
    Ok(())
}

/// Compare every non-git entry with core's init output, including ignored
/// files and symlinks. No user content is removed when this directory moves.
/// The repo may be unborn, or hold only the commit of those files that
/// `init_committed` makes: one root commit on the fleet branch, no other
/// refs, nothing staged or changed.
pub fn is_pristine_init(dir: &Path) -> anyhow::Result<bool> {
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
    fn files(
        root: &Path,
        dir: &Path,
        out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) -> anyhow::Result<bool> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let rel = path.strip_prefix(root)?;
            if rel == Path::new(".git") {
                continue;
            }
            let meta = path.symlink_metadata()?;
            if meta.is_symlink() {
                return Ok(false);
            }
            if meta.is_dir() {
                out.insert(rel.to_path_buf(), None);
                if !files(root, &path, out)? {
                    return Ok(false);
                }
            } else if meta.is_file() {
                out.insert(rel.to_path_buf(), Some(std::fs::read(path)?));
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let template = tempfile::tempdir()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the daemon does: fetch, then adopt from what was fetched.
    fn fetched<T>(dir: &Path, adopt: fn(&Path) -> anyhow::Result<T>) -> anyhow::Result<T> {
        fetch_all(dir, &[])?;
        adopt(dir)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare server repo with a baseline on `main`, and its URL.
    fn server(tmp: &Path) -> String {
        let seed = tmp.join("seed");
        git(tmp, &["init", "--quiet", "-b", "main", seed.to_str().unwrap()]);
        std::fs::write(seed.join("sk1ll.toml"), "").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "baseline"]);
        let bare = tmp.join("server.git");
        git(tmp, &["clone", "--quiet", "--bare", seed.to_str().unwrap(), bare.to_str().unwrap()]);
        bare.to_string_lossy().into_owned()
    }

    /// A repo made with `git init` on `branch`, not cloned, pointed at `remote`.
    fn independent(tmp: &Path, branch: &str, remote: &str) -> PathBuf {
        let dir = tmp.join(format!("own-{branch}"));
        git(tmp, &["init", "--quiet", "-b", branch, dir.to_str().unwrap()]);
        std::fs::write(dir.join("sk1ll.toml"), "# mine\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "--quiet", "-m", "my own root"]);
        git(&dir, &["remote", "add", REMOTE, remote]);
        dir
    }

    #[test]
    fn credentials_in_urls() {
        assert_eq!(
            without_credentials("https://ws:t0k@server.example:8443/git/repo.git"),
            "https://server.example:8443/git/repo.git"
        );
        assert_eq!(without_credentials("http://h/git/repo.git/"), "http://h/git/repo.git");
        assert!(has_credentials("https://ws:t0k@server.example/git/repo.git"));
        assert!(!has_credentials("https://server.example/git/a@b"));
    }

    #[test]
    fn an_unborn_clone_blocks_every_first_baseline_obstruction() {
        use std::os::unix::fs::symlink;
        for scenario in [
            "ancestor-file",
            "ancestor-symlink",
            "target-file",
            "target-directory",
            "target-symlink",
            "dangling-symlink",
        ] {
            for ignored in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let remote = tmp.path().join("empty.git");
                git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", remote.to_str().unwrap()]);
                let clone = tmp.path().join("clone");
                git(tmp.path(), &["clone", "--quiet", remote.to_str().unwrap(), clone.to_str().unwrap()]);
                Repo::init(&clone).unwrap();
                let seed = tmp.path().join("seed");
                Repo::init(&seed).unwrap();
                std::fs::create_dir_all(seed.join("personal/pdf")).unwrap();
                std::fs::write(seed.join("personal/pdf/SKILL.md"), "remote skill").unwrap();
                git(&seed, &["add", "-A"]);
                git(&seed, &["commit", "--quiet", "-m", "first baseline"]);
                git(&seed, &["push", "--quiet", remote.to_str().unwrap(), "main"]);
                let outside = tmp.path().join("outside");
                std::fs::create_dir(&outside).unwrap();
                std::fs::write(outside.join("notes"), "outside notes").unwrap();
                let ancestor = clone.join("personal/pdf");
                let target = ancestor.join("SKILL.md");
                if scenario.starts_with("target") || scenario == "dangling-symlink" {
                    std::fs::create_dir(&ancestor).unwrap();
                }
                match scenario {
                    "ancestor-file" => std::fs::write(&ancestor, "private notes").unwrap(),
                    "ancestor-symlink" => symlink(&outside, &ancestor).unwrap(),
                    "target-file" => std::fs::write(&target, "private notes").unwrap(),
                    "target-directory" => {
                        std::fs::create_dir(&target).unwrap();
                        std::fs::write(target.join("notes"), "private notes").unwrap();
                    }
                    "target-symlink" => symlink(outside.join("notes"), &target).unwrap(),
                    "dangling-symlink" => symlink(outside.join("missing"), &target).unwrap(),
                    _ => unreachable!(),
                }
                if ignored {
                    std::fs::write(clone.join(".git/info/exclude"), "personal/pdf\n").unwrap();
                }
                // Keep an index too, so this catches a reset that rewrites it.
                git(&clone, &["add", "sk1ll.toml"]);
                let head = std::fs::read(clone.join(".git/HEAD")).unwrap();
                let index = std::fs::read(clone.join(".git/index")).unwrap();
                assert!(
                    matches!(fetched(&clone, adopt_first_baseline).unwrap(), FirstBaseline::Blocked { items, .. }
                    if items == vec![ItemRef::Skill("pdf".into())]),
                    "{scenario}, ignored={ignored}"
                );
                assert_eq!(std::fs::read(clone.join(".git/HEAD")).unwrap(), head);
                assert_eq!(std::fs::read(clone.join(".git/index")).unwrap(), index);
                assert_eq!(rev(&clone, "HEAD").unwrap(), None);
                assert_eq!(std::fs::read_to_string(outside.join("notes")).unwrap(), "outside notes");
                match scenario {
                    "ancestor-file" => assert_eq!(std::fs::read_to_string(ancestor).unwrap(), "private notes"),
                    "ancestor-symlink" => assert_eq!(std::fs::read_link(ancestor).unwrap(), outside),
                    "target-file" => assert_eq!(std::fs::read_to_string(target).unwrap(), "private notes"),
                    "target-directory" => {
                        assert_eq!(std::fs::read_to_string(target.join("notes")).unwrap(), "private notes")
                    }
                    _ => assert!(std::fs::symlink_metadata(target).unwrap().is_symlink()),
                }
            }
        }
    }

    #[test]
    fn first_baseline_preserves_content_created_after_preflight() {
        for scenario in ["ancestor-file", "ancestor-symlink", "target-file", "target-directory", "target-symlink"] {
            let tmp = tempfile::tempdir().unwrap();
            let remote = server(tmp.path());
            let seed = tmp.path().join("seed");
            std::fs::create_dir_all(seed.join("personal/pdf")).unwrap();
            std::fs::create_dir_all(seed.join("instructions")).unwrap();
            std::fs::write(seed.join("instructions/first.md"), "published before the obstruction").unwrap();
            std::fs::write(seed.join("personal/pdf/SKILL.md"), "remote").unwrap();
            git(&seed, &["add", "-A"]);
            git(&seed, &["commit", "-qm", "skill"]);
            git(&seed, &["push", "--quiet", &remote, "main"]);
            let clone = tmp.path().join("clone");
            Repo::init(&clone).unwrap();
            git(&clone, &["remote", "add", "origin", &remote]);
            git(&clone, &["add", "sk1ll.toml"]);
            std::fs::write(clone.join(".git/info/exclude"), "personal/pdf\n").unwrap();
            let head = std::fs::read(clone.join(".git/HEAD")).unwrap();
            let index = std::fs::read(clone.join(".git/index")).unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("SKILL.md"), "private").unwrap();
            let ancestor = clone.join("personal/pdf");
            let target = ancestor.join("SKILL.md");
            let (a, t, o) = (ancestor.clone(), target.clone(), outside.clone());
            BEFORE_BASELINE_PUBLICATION.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    if scenario.starts_with("target") {
                        std::fs::create_dir(&a).unwrap();
                    }
                    match scenario {
                        "ancestor-file" => std::fs::write(a, "private").unwrap(),
                        "ancestor-symlink" => std::os::unix::fs::symlink(o, a).unwrap(),
                        "target-file" => std::fs::write(t, "private").unwrap(),
                        "target-directory" => {
                            std::fs::create_dir(&t).unwrap();
                            std::fs::write(t.join("notes"), "private").unwrap();
                        }
                        _ => std::os::unix::fs::symlink(o.join("SKILL.md"), t).unwrap(),
                    }
                }))
            });
            assert!(
                matches!(fetched(&clone, adopt_first_baseline).unwrap(), FirstBaseline::Blocked { items, .. }
                if items == vec![ItemRef::Skill("pdf".into())]),
                "{scenario}"
            );
            assert_eq!(std::fs::read(clone.join(".git/HEAD")).unwrap(), head);
            assert_eq!(std::fs::read(clone.join(".git/index")).unwrap(), index);
            assert_eq!(rev(&clone, "HEAD").unwrap(), None);
            assert_eq!(
                std::fs::read(clone.join("instructions/first.md")).unwrap(),
                b"published before the obstruction"
            );
            assert_eq!(std::fs::read(outside.join("SKILL.md")).unwrap(), b"private");
            match scenario {
                "ancestor-file" => assert_eq!(std::fs::read(ancestor).unwrap(), b"private"),
                "ancestor-symlink" => assert_eq!(std::fs::read_link(ancestor).unwrap(), outside),
                "target-file" => assert_eq!(std::fs::read(target).unwrap(), b"private"),
                "target-directory" => assert_eq!(std::fs::read(target.join("notes")).unwrap(), b"private"),
                _ => assert!(target.symlink_metadata().unwrap().is_symlink()),
            }
        }
    }

    #[test]
    fn a_committed_init_is_pristine_until_anything_else_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = server(tmp.path());
        let fresh = || {
            let repo = tempfile::tempdir_in(tmp.path()).unwrap().keep().join("repo");
            Repo::init_committed(&repo).unwrap();
            repo
        };
        let repo = fresh();
        assert!(is_pristine_init(&repo).unwrap());
        check_standalone_join(&repo, &remote).unwrap();

        let draft = fresh();
        std::fs::write(draft.join("instructions/git.md"), "draft").unwrap();
        let second = fresh();
        git(&second, &["commit", "--quiet", "--allow-empty", "-m", "more"]);
        let branch = fresh();
        git(&branch, &["branch", "kept"]);
        let removed = fresh();
        git(&removed, &["rm", "--quiet", "personal/.gitkeep"]);
        let ignored = fresh();
        std::fs::write(ignored.join(".sk1ll-stage-x"), "private").unwrap();
        for repo in [draft, second, branch, removed, ignored] {
            assert!(!is_pristine_init(&repo).unwrap(), "{}", repo.display());
            assert!(check_standalone_join(&repo, &remote).is_err(), "{}", repo.display());
        }
    }

    #[test]
    fn a_lone_init_commit_gives_way_to_an_unrelated_first_baseline_only() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = server(tmp.path());
        let repo = tmp.path().join("repo");
        Repo::init_committed(&repo).unwrap();
        git(&repo, &["remote", "add", "origin", &remote]);
        let baseline = git(&repo, &["ls-remote", &remote, "refs/heads/main"]);
        assert!(matches!(fetched(&repo, adopt_first_baseline_over_init).unwrap(), Some(FirstBaseline::Adopted(b))
            if baseline.starts_with(&b)));
        assert_eq!(rev(&repo, "HEAD").unwrap(), rev(&repo, "origin/main").unwrap());
        assert!(rev(&repo, "HEAD^").unwrap().is_none());

        // Sharing history with the remote, or holding more than the starter
        // files, is an ordinary pull.
        let shared = tmp.path().join("shared");
        git(tmp.path(), &["clone", "--quiet", repo.to_str().unwrap(), shared.to_str().unwrap()]);
        assert!(fetched(&shared, adopt_first_baseline_over_init).unwrap().is_none());
        let other = tmp.path().join("other");
        Repo::init_committed(&other).unwrap();
        std::fs::write(other.join("instructions/git.md"), "mine").unwrap();
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "--quiet", "--amend", "-m", "mine"]);
        git(&other, &["remote", "add", "origin", &remote]);
        assert!(fetched(&other, adopt_first_baseline_over_init).unwrap().is_none());
        assert!(rev(&other, "HEAD").unwrap().is_some());
    }

    #[test]
    fn standalone_join_refuses_ignored_content_in_unborn_clone() {
        for symlink in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let remote = server(tmp.path());
            let repo = tmp.path().join("local");
            git(tmp.path(), &["init", "-q", repo.to_str().unwrap()]);
            git(&repo, &["remote", "add", "origin", &remote]);
            std::fs::write(repo.join(".git/info/exclude"), "*\n").unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            if symlink {
                std::os::unix::fs::symlink(&outside, repo.join("personal")).unwrap();
            } else {
                std::fs::create_dir(repo.join("personal")).unwrap();
                std::fs::write(repo.join("personal/.gitkeep"), "private").unwrap();
            }
            assert!(check_standalone_join(&repo, &remote).is_err());
            assert!(!outside.join(".gitkeep").exists());
        }
    }

    #[test]
    fn an_unborn_clone_adopts_the_baseline_and_keeps_unrelated_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("empty.git");
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", remote.to_str().unwrap()]);
        let clone = tmp.path().join("clone");
        git(tmp.path(), &["clone", "--quiet", remote.to_str().unwrap(), clone.to_str().unwrap()]);
        Repo::init(&clone).unwrap();
        let seed = tmp.path().join("seed");
        Repo::init(&seed).unwrap();
        std::fs::create_dir_all(seed.join("personal/pdf")).unwrap();
        // NUL-delimited paths must survive whitespace, including newlines.
        std::fs::write(seed.join("personal/pdf/SKILL.md"), "remote skill").unwrap();
        std::fs::write(seed.join("personal/pdf/a\nb"), "attachment").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "first baseline"]);
        git(&seed, &["push", "--quiet", remote.to_str().unwrap(), "main"]);
        std::fs::write(clone.join("notes"), "private notes").unwrap();
        assert!(matches!(fetched(&clone, adopt_first_baseline).unwrap(), FirstBaseline::Adopted(_)));
        assert_eq!(std::fs::read_to_string(clone.join("personal/pdf/SKILL.md")).unwrap(), "remote skill");
        assert_eq!(std::fs::read_to_string(clone.join("personal/pdf/a\nb")).unwrap(), "attachment");
        assert_eq!(std::fs::read_to_string(clone.join("notes")).unwrap(), "private notes");
    }

    #[test]
    fn an_existing_clone_on_another_branch_is_refused_before_joining() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = server(tmp.path());
        let dir = independent(tmp.path(), "master", &remote);
        let err = format!("{:#}", check_repo_path(&dir, &remote).unwrap_err());
        assert!(err.contains("branch `master`") && err.contains("branch -m master main"), "{err}");
        assert!(ensure_clone(&dir, &remote, &[]).is_err());
        assert_eq!(git(&dir, &["symbolic-ref", "--short", "HEAD"]), "master");
    }

    #[test]
    fn an_adopted_clone_must_share_the_servers_history() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = server(tmp.path());
        // On `main`, but started on its own.
        let own = independent(tmp.path(), "main", &remote);
        check_repo_path(&own, &remote).unwrap();
        let err = format!("{:#}", check_shares_history(&own, &[]).unwrap_err());
        assert!(err.contains("shares no history"), "{err}");

        // A real clone, even one with commits of its own, is fine.
        let clone = tmp.path().join("clone");
        git(tmp.path(), &["clone", "--quiet", &remote, clone.to_str().unwrap()]);
        std::fs::write(clone.join("notes.txt"), "n\n").unwrap();
        git(&clone, &["add", "-A"]);
        git(&clone, &["commit", "--quiet", "-m", "local"]);
        assert_eq!(ensure_clone(&clone, &remote, &[]).unwrap(), Setup::Adopted);
        check_shares_history(&clone, &[]).unwrap();

        // A server must have a Baseline commit before a clone can join.
        let empty = tmp.path().join("empty.git");
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
        git(&own, &["remote", "set-url", REMOTE, empty.to_str().unwrap()]);
        assert!(check_shares_history(&own, &[]).unwrap_err().to_string().contains("no commits on `main`"));
    }

    /// A clone runs no filter driver, not even one git's configuration
    /// names only for the clone's own git directory (`includeIf.gitdir`),
    /// and a Baseline that writes a file through one is refused.
    #[test]
    fn a_clone_runs_no_filter_driver_a_conditional_include_names() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let seed = tmp.join("seed");
        git(&tmp, &["init", "--quiet", "-b", "main", seed.to_str().unwrap()]);
        std::fs::write(seed.join("sk1ll.toml"), "").unwrap();
        std::fs::write(seed.join(".gitattributes"), "*.md filter=x\n").unwrap();
        std::fs::write(seed.join("notes.md"), "notes\n").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "baseline"]);
        let remote = tmp.join("server.git");
        git(&tmp, &["clone", "--quiet", "--bare", seed.to_str().unwrap(), remote.to_str().unwrap()]);
        let marker = tmp.join("ran");
        let filter = tmp.join("filter.gitconfig");
        let cmd = format!("touch '{}'; cat", marker.display());
        std::fs::write(&filter, format!("[filter \"x\"]\n\tsmudge = {cmd}\n\tclean = {cmd}\n")).unwrap();
        // Applies to any git directory under `tmp`: the clone's, not the
        // directory it is made from.
        let global = tmp.join("global.gitconfig");
        let include = format!("[includeIf \"gitdir:{}/**\"]\n\tpath = {}\n", tmp.display(), filter.display());
        std::fs::write(&global, include).unwrap();
        let env = [("GIT_CONFIG_GLOBAL".to_string(), global.to_string_lossy().into_owned())];
        let dir = tmp.join("clone");
        let error = ensure_clone(&dir, remote.to_str().unwrap(), &env).unwrap_err().to_string();
        assert!(error.contains("notes.md") && error.contains("filter driver"), "{error}");
        assert!(!marker.exists(), "a filter driver ran");
        assert!(!dir.exists(), "a refused clone is not moved into place");
        // The include does name the driver where git checks out.
        let plain = tmp.join("plain");
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", remote.to_str().unwrap(), plain.to_str().unwrap()])
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(marker.exists(), "the test's driver is not configured as git reads it");
    }

    /// Without a filter attribute, the clone is checked out in full.
    #[test]
    fn a_clone_is_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = server(tmp.path());
        let dir = tmp.path().join("clone");
        assert_eq!(ensure_clone(&dir, &remote, &[]).unwrap(), Setup::Cloned);
        assert!(dir.join("sk1ll.toml").is_file());
        assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    }
}
