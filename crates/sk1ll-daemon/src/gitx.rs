//! Git plumbing the daemon needs beyond `sk1ll_core::Repo`: reading refs,
//! ancestry, unpushed items, and setting up the local clone. Never prompts.
//!
//! Every command, clone included, runs through [`sk1ll_core::git::Git`], so it
//! has the same execution deadline as core's own git calls
//! (`SK1LL_GIT_TIMEOUT` / `SK1LL_GIT_LOCAL_TIMEOUT`): a server that accepts the
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

/// Diff two text snapshots using Git's unified diff, without touching the repo.
pub(crate) fn diff_text(before: Option<&str>, after: &str) -> anyhow::Result<String> {
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

/// `refs/remotes/origin/main`: the server's baseline as last fetched or pushed.
pub(crate) fn remote_head(dir: &Path) -> anyhow::Result<Option<String>> {
    rev(dir, &format!("refs/remotes/{REMOTE}/{FLEET_BRANCH}"))
}

/// Fetches every branch of origin (pruning the ones it no longer has), so
/// `refs/remotes/origin/*` is exactly what the server has.
fn fetch_all(dir: &Path, env: Env) -> anyhow::Result<()> {
    let refspec = format!("+refs/heads/*:refs/remotes/{REMOTE}/*");
    run_env(dir, &["fetch", "--quiet", "--prune", REMOTE, &refspec], env).map(drop)
}

fn no_fleet_branch(others: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "the server has no `{FLEET_BRANCH}` branch (it has {}); its baseline must be on it",
        others.join(", ")
    )
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

/// The server baseline the working tree is built on: origin's branch if `HEAD`
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

/// The first baseline can clash with files even though HEAD is unborn.
pub(crate) enum FirstBaseline {
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
pub(crate) fn adopt_first_baseline(dir: &Path) -> anyhow::Result<FirstBaseline> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fetch_all(dir, &[])?;
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
    let directory = sk1ll_core::placement::Directory::open(dir, false)?;
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
    run(dir, &["reset", "--quiet", "--mixed", &remote])?;
    Ok(FirstBaseline::Adopted(remote))
}

/// A repo holding only the commit `init_committed` makes takes a first
/// baseline of unrelated history as an unborn init would: that commit is
/// dropped, so the starter files are draft again, and the baseline adopted.
/// Unless it is adopted, the commit is restored. `None` when this doesn't apply.
pub(crate) fn adopt_first_baseline_over_init(dir: &Path) -> anyhow::Result<Option<FirstBaseline>> {
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
    fetch_all(dir, &[])?;
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
    /// The server's repo is empty: cloned it and laid out a new skills repo as draft.
    Initialized,
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
/// commits, or an empty server (only older servers start without a commit), passes.
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
        "{}'s `{FLEET_BRANCH}` shares no history with the server's `{FLEET_BRANCH}` (it was started on its own, not \
         cloned from the server), so its commits can't be pulled or pushed; move it aside and the daemon clones the server's \
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
        // An empty server (older servers start without a commit): start a skills
        // repo on `main`; it is pushed by the first commit.
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

/// Whether this repo defines its own origin remote.
pub(crate) fn has_origin(dir: &Path) -> anyhow::Result<bool> {
    Ok(try_run(dir, &["remote", "get-url", "origin"])?.is_some())
}

/// A standalone repo may join only when it is an untouched init, or an
/// unchanged clone of this server without any local commits to preserve.
pub(crate) fn check_standalone_join(dir: &Path, remote: &str) -> anyhow::Result<()> {
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
pub(crate) fn is_pristine_init(dir: &Path) -> anyhow::Result<bool> {
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
    fn empty_server_clone_blocks_every_first_baseline_obstruction() {
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
                assert_eq!(ensure_clone(&clone, remote.to_str().unwrap(), &[]).unwrap(), Setup::Initialized);
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
                    matches!(adopt_first_baseline(&clone).unwrap(), FirstBaseline::Blocked { items, .. }
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
                matches!(adopt_first_baseline(&clone).unwrap(), FirstBaseline::Blocked { items, .. }
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
        assert!(matches!(adopt_first_baseline_over_init(&repo).unwrap(), Some(FirstBaseline::Adopted(b))
            if baseline.starts_with(&b)));
        assert_eq!(rev(&repo, "HEAD").unwrap(), rev(&repo, "origin/main").unwrap());
        assert!(rev(&repo, "HEAD^").unwrap().is_none());

        // Sharing history with the remote, or holding more than the starter
        // files, is an ordinary pull.
        let shared = tmp.path().join("shared");
        git(tmp.path(), &["clone", "--quiet", repo.to_str().unwrap(), shared.to_str().unwrap()]);
        assert!(adopt_first_baseline_over_init(&shared).unwrap().is_none());
        let other = tmp.path().join("other");
        Repo::init_committed(&other).unwrap();
        std::fs::write(other.join("instructions/git.md"), "mine").unwrap();
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "--quiet", "--amend", "-m", "mine"]);
        git(&other, &["remote", "add", "origin", &remote]);
        assert!(adopt_first_baseline_over_init(&other).unwrap().is_none());
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
    fn empty_server_clone_adopts_an_unobstructed_baseline_and_keeps_unrelated_drafts() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("empty.git");
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", remote.to_str().unwrap()]);
        let clone = tmp.path().join("clone");
        assert_eq!(ensure_clone(&clone, remote.to_str().unwrap(), &[]).unwrap(), Setup::Initialized);
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
        assert!(matches!(adopt_first_baseline(&clone).unwrap(), FirstBaseline::Adopted(_)));
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

        // An empty (older) server has no history to share yet.
        let empty = tmp.path().join("empty.git");
        git(tmp.path(), &["init", "--quiet", "--bare", "-b", "main", empty.to_str().unwrap()]);
        git(&own, &["remote", "set-url", REMOTE, empty.to_str().unwrap()]);
        check_shares_history(&own, &[]).unwrap();
    }
}
