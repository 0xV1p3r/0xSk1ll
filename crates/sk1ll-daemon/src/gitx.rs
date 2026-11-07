//! Git plumbing the daemon needs beyond `sk1ll_core::Repo`: reading refs,
//! ancestry, unpushed items, and setting up the local clone. Never prompts.
//!
//! Every command, clone included, runs through [`sk1ll_core::git::Git`], so it
//! has the same execution deadline as core's own git calls
//! (`SK1LL_GIT_TIMEOUT` / `SK1LL_GIT_LOCAL_TIMEOUT`): a hub that accepts the
//! connection and then never answers can't hold the daemon forever.
//!
//! No URL here carries credentials (see [`crate::auth`]), and error messages
//! are redacted all the same.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sk1ll_core::draft::{FLEET_BRANCH, REMOTE};
use sk1ll_core::git::Git;
use sk1ll_core::{ItemRef, Repo};

use crate::auth::redact;
use crate::state::{has_credentials, without_credentials};

type Env<'a> = &'a [(String, String)];

fn command(dir: &Path, args: &[&str], env: Env) -> Git {
    let mut git = Git::new(dir).env("GIT_TERMINAL_PROMPT", "0").args(args);
    for (k, v) in env {
        git = git.env(k, v);
    }
    git
}

/// A core git error with the URL-ish parts redacted.
fn redacted(e: sk1ll_core::Error) -> anyhow::Error {
    match e {
        sk1ll_core::Error::Git { args, stderr } => {
            anyhow::anyhow!("`git {}` failed: {}", redact(&args), redact(&stderr))
        }
        e => anyhow::anyhow!("{}", redact(&e.to_string())),
    }
}

fn output(dir: &Path, args: &[&str], env: Env) -> anyhow::Result<Output> {
    command(dir, args, env).output().map_err(redacted)
}

fn failed(args: &[&str], out: &Output) -> anyhow::Error {
    anyhow::anyhow!("`git {}` failed: {}", redact(&args.join(" ")), redact(String::from_utf8_lossy(&out.stderr).trim()))
}

/// Runs git; non-zero is an error. Returns trimmed stdout.
pub(crate) fn run(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
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

pub(crate) fn rev(dir: &Path, name: &str) -> anyhow::Result<Option<String>> {
    try_run(dir, &["rev-parse", "--verify", "--quiet", &format!("{name}^{{commit}}")])
}

pub(crate) fn branch(dir: &Path) -> anyhow::Result<Option<String>> {
    try_run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

/// `refs/remotes/origin/main`: the hub's baseline as last fetched or pushed.
pub(crate) fn remote_head(dir: &Path) -> anyhow::Result<Option<String>> {
    rev(dir, &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))
}

/// Fetches every branch of origin (pruning the ones it no longer has), so
/// `refs/remotes/origin/*` is exactly what the hub has.
fn fetch_all(dir: &Path, env: Env) -> anyhow::Result<()> {
    let refspec = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
    run_env(dir, &["fetch", "--quiet", "--prune", REMOTE, &refspec], env).map(drop)
}

fn no_fleet_branch(others: &[String]) -> anyhow::Error {
    anyhow::anyhow!("the hub has no `{FLEET_BRANCH}` branch (it has {}); its baseline must be on it", others.join(", "))
}

/// The branches origin had at the last [`fetch_all`].
fn remote_branches(dir: &Path) -> anyhow::Result<Vec<String>> {
    let prefix = format!("refs/remotes/{REMOTE}/");
    let refs = run(dir, &["for-each-ref", "--format=%(refname)", &prefix])?;
    Ok(refs.lines().filter_map(|r| r.strip_prefix(&prefix)).filter(|b| *b != "HEAD").map(str::to_string).collect())
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> anyhow::Result<bool> {
    Ok(output(dir, &["merge-base", "--is-ancestor", a, b], &[])?.status.success())
}

/// The hub baseline the working tree is built on: origin's branch if `HEAD`
/// contains it, else where `HEAD` forked from it (Blocked: the old baseline).
pub(crate) fn baseline(dir: &Path) -> anyhow::Result<Option<String>> {
    let (Some(remote), Some(head)) = (remote_head(dir)?, rev(dir, "HEAD")?) else {
        return Ok(None);
    };
    if is_ancestor(dir, &remote, &head)? {
        return Ok(Some(remote));
    }
    try_run(dir, &["merge-base", &head, &remote])
}

/// Whether `HEAD` has commits origin doesn't.
pub(crate) fn has_unpushed(dir: &Path) -> anyhow::Result<bool> {
    let Some(head) = rev(dir, "HEAD")? else {
        return Ok(false);
    };
    match remote_head(dir)? {
        Some(remote) => Ok(!is_ancestor(dir, &head, &remote)?),
        None => Ok(true),
    }
}

/// Items touched by commits in `HEAD` that origin doesn't have.
pub(crate) fn unpushed_items(dir: &Path) -> anyhow::Result<Vec<ItemRef>> {
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

/// A repo whose `HEAD` has no commit yet (cloned from an empty hub) takes
/// the hub's baseline once it has one: `HEAD` moves to it without touching
/// the working tree, and files the baseline has that the machine lacks are
/// checked out. Local files that differ stay as draft. Returns the commit,
/// `None` if the hub is still empty.
pub(crate) fn adopt_first_baseline(dir: &Path) -> anyhow::Result<Option<String>> {
    fetch_all(dir, &[])?;
    let Some(remote) = remote_head(dir)? else {
        let others = remote_branches(dir)?;
        if others.is_empty() {
            return Ok(None);
        }
        return Err(no_fleet_branch(&others));
    };
    // The baseline always lives on the fleet branch, whatever the unborn HEAD was called.
    run(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{FLEET_BRANCH}")])?;
    run(dir, &["reset", "--quiet", "--mixed", &remote])?;
    let missing = run(dir, &["ls-files", "--deleted"])?;
    let missing: Vec<&str> = missing.lines().filter(|l| !l.is_empty()).collect();
    if !missing.is_empty() {
        let mut args = vec!["checkout", "--quiet", "--"];
        args.extend(missing);
        run(dir, &args)?;
    }
    Ok(Some(remote))
}

/// What [`ensure_clone`] found at the repo path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// Cloned the hub's repo.
    Cloned,
    /// The hub's repo is empty: cloned it and laid out a new skills repo as draft.
    Initialized,
    /// An existing clone of the hub's repo; its origin was updated.
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
        bail!("{} exists and is not empty or a clone of the hub's repo; pick another --repo", dir.display());
    }
    let origin = try_run(dir, &["remote", "get-url", REMOTE])?;
    match origin {
        Some(url) if without_credentials(&url) == without_credentials(remote) => check_fleet_branch(dir),
        Some(url) => bail!(
            "{} is a clone of {}, not of the hub ({}); pick another --repo",
            dir.display(),
            without_credentials(&url),
            without_credentials(remote)
        ),
        None => bail!("{} is a git repo without an `{REMOTE}`; pick another --repo", dir.display()),
    }
}

/// An existing clone must be on the fleet branch: the hub's baseline is
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

/// For an existing clone being adopted at join: fetches the hub and checks
/// that the clone's history and the hub's `main` share a commit, so the
/// first pull or push can't go off on a history of its own. A clone without
/// commits, or an empty hub (only older hubs start without a commit), passes.
/// `env` authenticates git, as for [`ensure_clone`].
pub fn check_shares_history(dir: &Path, env: &[(String, String)]) -> anyhow::Result<()> {
    check_fleet_branch(dir)?;
    fetch_all(dir, env)?;
    let Some(head) = rev(dir, "HEAD")? else {
        return Ok(());
    };
    let Some(remote) = remote_head(dir)? else {
        let others = remote_branches(dir)?;
        if others.is_empty() {
            return Ok(());
        }
        return Err(no_fleet_branch(&others));
    };
    if output(dir, &["merge-base", &head, &remote], &[])?.status.success() {
        return Ok(());
    }
    bail!(
        "{}'s `{FLEET_BRANCH}` shares no history with the hub's `{FLEET_BRANCH}` (it was started on its own, not \
         cloned from the hub), so its commits can't be pulled or pushed; move it aside and the daemon clones the hub's \
         repo there, or pick another --repo",
        dir.display()
    )
}

/// Makes `dir` a clone of `remote` (see [`check_repo_path`]). `env` is how
/// git authenticates (see [`crate::auth::git_credentials_env`]); origin
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
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let parent: PathBuf = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| ".".into());
        let target = dir.to_string_lossy();
        run_env(&parent, &["clone", "--quiet", "--origin", REMOTE, &remote, &target], env)?;
        Setup::Cloned
    };
    private_config(dir)?;
    if rev(dir, "HEAD")?.is_none() {
        // An empty hub (older hubs start without a commit): start a skills
        // repo on `main`; it is pushed by the first promote.
        if branch(dir)?.as_deref() != Some(FLEET_BRANCH) {
            run(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{FLEET_BRANCH}")])?;
        }
        if !dir.join(sk1ll_core::manifest::MANIFEST_FILE).exists() {
            Repo::init(dir)?;
            return Ok(if setup == Setup::Cloned { Setup::Initialized } else { setup });
        }
    }
    Ok(setup)
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A bare hub repo with a baseline on `main`, and its URL.
    fn hub(tmp: &Path) -> String {
        let seed = tmp.join("seed");
        git(tmp, &["init", "--quiet", "-b", "main", seed.to_str().unwrap()]);
        std::fs::write(seed.join("sk1ll.toml"), "").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "--quiet", "-m", "baseline"]);
        let bare = tmp.join("hub.git");
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
    fn an_existing_clone_on_another_branch_is_refused_before_joining() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = hub(tmp.path());
        let dir = independent(tmp.path(), "master", &remote);
        let err = format!("{:#}", check_repo_path(&dir, &remote).unwrap_err());
        assert!(err.contains("branch `master`") && err.contains("branch -m master main"), "{err}");
        assert!(ensure_clone(&dir, &remote, &[]).is_err());
        assert_eq!(git(&dir, &["symbolic-ref", "--short", "HEAD"]), "master");
    }

    #[test]
    fn an_adopted_clone_must_share_the_hubs_history() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = hub(tmp.path());
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

        // An empty (older) hub has no history to share yet.
        let empty = tmp.path().join("empty.git");
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
        git(&own, &["remote", "set-url", REMOTE, empty.to_str().unwrap()]);
        check_shares_history(&own, &[]).unwrap();
    }
}
