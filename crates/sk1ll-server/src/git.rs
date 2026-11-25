//! The fleet's bare repo: setup, reading the baseline, and serving git smart
//! HTTP by running `git http-backend` as a CGI program.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::StreamExt;
use sk1ll_core::instructions::INSTRUCTIONS_DIR;
use sk1ll_core::manifest::MANIFEST_FILE;
use sk1ll_core::skill::{IMPORTED_DIR, PERSONAL_DIR};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::error::ApiError;
use crate::{Caller, Server};

/// Creates the bare repo (default branch `main`, pushes over HTTP allowed)
/// unless it exists.
pub fn init_bare(repo: &Path) -> anyhow::Result<()> {
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let out = setup_git().args(args).output().context("running git")?;
        if !out.status.success() {
            bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    };
    let path = repo.to_string_lossy();
    let created = !repo.join("HEAD").is_file();
    if created {
        run(&["init", "--quiet", "--bare", "--initial-branch=main", "--template=", &path])?;
    }
    // Applied on every start, so repos created by older servers get them too.
    run(&["-C", &path, "config", "http.receivepack", "true"])?;
    run(&["-C", &path, "config", "receive.denyNonFastForwards", "true"])?;
    run(&["-C", &path, "config", "receive.denyDeletes", "true"])?;
    // Replacement objects would let a push change the tree the server reads
    // for a commit without moving HEAD. Every server-side read also sets
    // GIT_NO_REPLACE_OBJECTS; this covers git run in the repo by hand.
    run(&["-C", &path, "config", "core.useReplaceRefs", "false"])?;
    // Left in place, not deleted: nothing reads them any more.
    let out = setup_git().args(["-C", &path, "for-each-ref", "--format=%(refname)", "refs/replace/"]).output()?;
    let replaced = String::from_utf8_lossy(&out.stdout).lines().count();
    if replaced > 0 {
        tracing::warn!("{}: ignoring {replaced} replacement ref(s) under refs/replace/", repo.display());
    }
    std::fs::create_dir_all(repo.join("hooks")).context("creating bare repo hooks directory")?;
    if created {
        tracing::info!("created {}", repo.display());
    }
    Ok(())
}

/// Decides the baseline branch, once, after [`init_baseline`]: the branch
/// HEAD names, which must be under `refs/heads/`. Only that branch takes
/// pushes: receive-pack refuses to create or update a hidden ref (the last
/// matching entry wins), and a pre-receive hook, written on every start,
/// refuses every other ref too, since hiding matches by prefix and so lets
/// `<branch>/x` through. Changing HEAD takes a restart. With `check`, the
/// hook also runs that `sk1ll-server` binary as `--check-push <commit>` on
/// each new baseline (see [`check_push`]).
pub fn configure_pushes(repo: &Path, check: Option<&Path>) -> anyhow::Result<String> {
    let branch = baseline_branch(repo)?;
    let path = repo.to_string_lossy();
    for args in [
        ["-C", &path, "config", "--replace-all", "receive.hideRefs", "refs"],
        ["-C", &path, "config", "--add", "receive.hideRefs", &format!("!{branch}")],
    ] {
        let out = setup_git().args(args).output().context("running git")?;
        if !out.status.success() {
            bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let partial = repo.join("hooks/pre-receive.sk1ll-tmp");
    std::fs::write(&partial, pre_receive_hook(&branch, check)).context("writing the pre-receive hook")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755))
            .context("making the pre-receive hook executable")?;
    }
    std::fs::rename(&partial, repo.join("hooks/pre-receive")).context("installing the pre-receive hook")?;
    Ok(branch)
}

/// The branch HEAD names. Refuses a detached HEAD or one outside `refs/heads/`.
fn baseline_branch(repo: &Path) -> anyhow::Result<String> {
    let out = setup_git().arg("-C").arg(repo).args(["symbolic-ref", "--quiet", "HEAD"]).output()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let fix = format!(
        "point it at the baseline branch with `git -C {} symbolic-ref HEAD refs/heads/<branch>` and start the server again",
        repo.display()
    );
    if !out.status.success() {
        bail!("{}: HEAD is detached, so there is no baseline branch to serve; {fix}", repo.display());
    }
    if !branch.starts_with("refs/heads/") {
        bail!("{}: HEAD names `{branch}`, not a branch under refs/heads/; {fix}", repo.display());
    }
    Ok(branch)
}

/// A pre-receive hook that refuses every ref update but one to `branch`,
/// and with `check`, one whose new commit that binary's `--check-push`
/// refuses. Run by receive-pack in the bare repo, before the refs move and
/// with the pushed objects still quarantined, which the check reads.
fn pre_receive_hook(branch: &str, check: Option<&Path>) -> String {
    let branch = shell_quote(branch);
    let check = check.map_or_else(String::new, |program| {
        format!("\t{} --check-push \"$new\" || exit 1\n", shell_quote(&program.to_string_lossy()))
    });
    format!(
        "#!/bin/sh\n\
         # Written by sk1ll-server on every start; changes here are lost.\n\
         while read -r old new ref; do\n\
         \tif [ \"$ref\" != {branch} ]; then\n\
         \t\tprintf 'sk1ll-server: only %s takes pushes, not %s\\n' {branch} \"$ref\" >&2\n\
         \t\texit 1\n\
         \tfi\n\
         {check}\
         done\n"
    )
}

/// Refuses `commit` as the new baseline of the bare repo `repo` unless every
/// item in its tree (personal skill, Rule, Import) has an Item ID of its own,
/// so the baseline's Item IDs are always complete (ADR 0006). The error names
/// each item and how to repair it.
///
/// It also refuses a tree whose `.gitattributes` give a file a git filter
/// driver (`filter=lfs`): the skills repo supports none, and Machines
/// refuse to pull such a Baseline.
pub async fn check_push(repo: &Path, commit: &str) -> anyhow::Result<()> {
    let inventory = item_ids_at(repo, commit).await.with_context(|| format!("cannot read the Item IDs of {commit}"))?;
    let filtered =
        filter_driver_paths(repo, commit).await.with_context(|| format!("cannot read the attributes of {commit}"))?;
    // The same rules a commit checks its tree by.
    if let Some(reason) = sk1ll_core::baseline_check::refusal(inventory.id_problems().as_ref(), &filtered) {
        bail!("refusing {commit}: {reason}");
    }
    Ok(())
}

/// The files of `commit` whose `filter` attribute, read from its own
/// `.gitattributes`, names a driver. `set`, `unset` and `unspecified` are
/// no driver to `git check-attr` (it prints them for `filter`, `-filter` and
/// none too), and the server configures none of that name.
async fn filter_driver_paths(repo: &Path, commit: &str) -> anyhow::Result<Vec<String>> {
    let args = ["ls-tree", "-r", "-z", "--name-only", "--full-tree", commit];
    let Some(paths) = git_bytes(repo, &args, None).await? else { bail!("cannot list tree of {commit}") };
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let source = format!("--source={commit}");
    let args = ["check-attr", &source, "-z", "--stdin", "filter"];
    let Some(out) = git_bytes(repo, &args, Some(paths)).await? else {
        bail!("cannot check the attributes of {commit}")
    };
    let none = std::collections::BTreeSet::new();
    Ok(sk1ll_core::git::filtered_paths(&out, &none).iter().map(|p| p.to_string_lossy().into_owned()).collect())
}

/// Bootstrap commands must not inherit identity, signing, hooks or templates
/// from the environment or the user's Git configuration.
fn setup_git() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_AUTHOR_NAME", "server")
        .env("GIT_AUTHOR_EMAIL", "sk1ll@server")
        .env("GIT_COMMITTER_NAME", "server")
        .env("GIT_COMMITTER_EMAIL", "sk1ll@server")
        .stdin(Stdio::null());
    cmd
}

/// Gives a fresh or previously empty server one shared root commit. Repositories
/// with any refs (even without main), or a detached HEAD, are left untouched.
pub fn init_baseline(repo: &Path) -> anyhow::Result<()> {
    let run = |dir: &Path, args: &[&str]| -> anyhow::Result<String> {
        let out = setup_git().arg("-C").arg(dir).args(args).output().context("running bootstrap git")?;
        if !out.status.success() {
            bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    };
    if !run(repo, &["for-each-ref", "--format=%(refname)"])?.is_empty() {
        return Ok(());
    }
    if setup_git().arg("-C").arg(repo).args(["rev-parse", "--verify", "HEAD^{commit}"]).output()?.status.success() {
        return Ok(());
    }
    let working = tempfile::tempdir().context("creating initial baseline working directory")?;
    // Init Git first so Repo::init only writes the standard skeleton and
    // never runs its own Git command with inherited global configuration.
    run(working.path(), &["init", "--quiet", "--initial-branch=main", "--template="])?;
    sk1ll_core::Repo::init(working.path()).context("creating initial baseline skeleton")?;
    run(working.path(), &["add", "--all"])?;
    run(working.path(), &["commit", "--quiet", "-m", "Initialize fleet baseline"])?;
    run(repo, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    run(repo, &["fetch", "--quiet", "--no-tags", "--", &working.path().to_string_lossy(), "main:refs/heads/main"])?;
    tracing::info!("created initial baseline in {}", repo.display());
    Ok(())
}

/// How long reading the repo may take. Some reads run with the push lock
/// held, so a stuck one must not hold up every later push.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How much output a repo read may produce before it is stopped and fails.
const INSPECT_OUTPUT: usize = 64 << 20;

/// Runs git in `repo`, ignoring replacement objects; `Ok(None)` if it exited
/// non-zero. An error if it outlives [`INSPECT_TIMEOUT`] or writes more than
/// [`INSPECT_OUTPUT`].
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    git_bytes(repo, args, None).await.map(|out| out.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
}

/// [`git`] with `stdin` as its input, returning raw output.
async fn git_bytes(repo: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> anyhow::Result<Option<Vec<u8>>> {
    inspect("git".as_ref(), repo, args, stdin, INSPECT_TIMEOUT, INSPECT_OUTPUT).await
}

/// Kills a read's process group unless disarmed, so a read that fails or is
/// dropped (its caller cancelled) leaves no git or child of it running. Kept
/// armed only while the direct child is unreaped, or the group's id could
/// already name another group.
#[cfg(unix)]
struct GroupGuard(Option<libc::pid_t>);

#[cfg(unix)]
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            let _ = signal_group(pgid, libc::SIGKILL);
        }
    }
}

/// [`git_bytes`] with `program` as git, stopped (with any children) after
/// `timeout` or past `max_output` bytes of output.
async fn inspect(
    program: &std::ffi::OsStr,
    repo: &Path,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_output: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut cmd = Command::new(program);
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.as_std_mut().process_group(0);
    let mut child = cmd.spawn().context("running git")?;
    #[cfg(unix)]
    let pgid = child.id().expect("newly spawned child has a pid") as libc::pid_t;
    #[cfg(unix)]
    let mut guard = GroupGuard(Some(pgid));
    let mut stdout = child.stdout.take().expect("piped");
    let input = child.stdin.take();
    let run = async {
        let write = async {
            if let (Some(mut input), Some(bytes)) = (input, stdin) {
                // A git that exits early closes its end: what it read decides.
                let _ = input.write_all(&bytes).await;
            }
            Ok::<_, anyhow::Error>(())
        };
        let mut out = Vec::new();
        let read = async {
            let mut chunk = vec![0; 64 << 10];
            loop {
                let n = stdout.read(&mut chunk).await.context("reading git output")?;
                if n == 0 {
                    return Ok(());
                }
                if out.len() + n > max_output {
                    bail!("`git {}` wrote more than {max_output} bytes", args.join(" "));
                }
                out.extend_from_slice(&chunk[..n]);
            }
        };
        let wait = async { child.wait().await.context("waiting for git") };
        let ((), (), status) = tokio::try_join!(write, read, wait)?;
        Ok::<_, anyhow::Error>((status, out))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => {
            let (status, out) = result?;
            #[cfg(unix)]
            {
                guard.0 = None;
            }
            Ok(status.success().then_some(out))
        }
        Err(_) => {
            #[cfg(unix)]
            let stopped = terminate_group(&mut child, pgid).await;
            #[cfg(unix)]
            if stopped.is_ok() {
                guard.0 = None;
            }
            #[cfg(not(unix))]
            let stopped = child.kill().await.map_err(anyhow::Error::from);
            if let Err(e) = stopped {
                tracing::error!("stopping timed-out `git {}`: {e:#}", args.join(" "));
            }
            bail!("`git {}` timed out after {timeout:?}", args.join(" "))
        }
    }
}

/// Where the baseline branch points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Commit(String),
    /// The branch doesn't exist yet (checked, not assumed from a failed read).
    Unborn,
}

/// The baseline: the commit `branch` (the one [`configure_pushes`] decided)
/// points at. An error if that can't be told, including when the branch
/// exists but resolving it failed.
pub async fn head(repo: &Path, branch: &str) -> anyhow::Result<Head> {
    if let Some(commit) = git(repo, &["rev-parse", "--verify", "--quiet", &format!("{branch}^{{commit}}")]).await? {
        return Ok(Head::Commit(commit.trim().to_string()));
    }
    let Some(refs) = git(repo, &["for-each-ref", "--format=%(refname)", branch]).await? else {
        bail!("cannot list {branch}")
    };
    if refs.lines().any(|r| r == branch) {
        bail!("cannot resolve {branch} to a commit");
    }
    Ok(Head::Unborn)
}

/// A file's contents at `commit`, `None` if it doesn't exist there.
pub async fn show(repo: &Path, commit: &str, path: &str) -> anyhow::Result<Option<String>> {
    git(repo, &["show", &format!("{commit}:{path}")]).await
}

/// Reads each metadata blob separately. Rule claims stop at the end of
/// frontmatter, so their content and other objects do not share a read cap.
pub async fn item_ids_at(repo: &Path, commit: &str) -> anyhow::Result<sk1ll_core::item_id::ItemInventory> {
    let args = ["ls-tree", "-r", "-z", commit, "--", PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR, MANIFEST_FILE];
    let Some(out) = git_bytes(repo, &args, None).await? else { bail!("cannot list tree of {commit}") };
    let mut entries = BTreeMap::new();
    let mut errors = Vec::new();
    for row in out.split(|&b| b == 0).filter(|row| !row.is_empty()) {
        let row = match str::from_utf8(row) {
            Ok(row) => row,
            Err(e) => {
                errors.push(format!("non-UTF-8 path in {commit}: {e}"));
                continue;
            }
        };
        let (metadata, path) = row.split_once('\t').context("malformed ls-tree entry")?;
        let mut metadata = metadata.split_whitespace();
        let mode = metadata.next().context("missing mode")?;
        let oid = metadata.nth(1).context("missing object ID")?;
        entries.insert(path.to_string(), (mode, oid));
    }
    let paths: Vec<_> = entries.keys().cloned().collect();
    let mut files = BTreeMap::new();
    for path in sk1ll_core::item_id::id_sources(&paths) {
        let (mode, oid) = entries[path];
        let claim = if matches!(mode, "100644" | "100755") {
            claim_blob(repo, oid, path).await.map_err(|e| e.to_string())
        } else {
            Err("expected a regular file".into())
        };
        files.insert(path.to_string(), claim);
    }
    let mut inventory = sk1ll_core::item_id::tree_inventory(&paths, &files);
    inventory.errors.extend(errors);
    Ok(inventory)
}

async fn claim_blob(repo: &Path, oid: &str, path: &str) -> anyhow::Result<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", oid])
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("reading Item ID blob")?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut source = sk1ll_core::item_id::ClaimSource::new(path);
    tokio::time::timeout(INSPECT_TIMEOUT, async {
        let mut chunk = [0; 8192];
        loop {
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                anyhow::ensure!(child.wait().await?.success(), "could not read Item ID blob {oid}");
                break;
            }
            if source.push(&chunk[..n]).map_err(anyhow::Error::msg)? {
                child.kill().await?;
                break;
            }
        }
        Ok::<_, anyhow::Error>(source.finish())
    })
    .await
    .context("reading Item ID blob timed out")?
}

/// How the import check runs `git ls-remote`.
pub struct RemoteCheck<'a> {
    /// The only transports used (git's `GIT_ALLOW_PROTOCOL` list): the URL
    /// comes from a pushed manifest, so `ext::` and local paths must not
    /// reach git.
    pub protocols: &'a str,
    /// Set with an import host allowlist, so git contacts the host name that
    /// was checked: HTTP redirects fail the check, and git and ssh run without
    /// the server's configuration, whose `url.*.insteadOf` rules or ssh
    /// `Host` sections could send the URL elsewhere.
    pub isolated: bool,
    /// The ssh program an isolated check runs.
    pub ssh: &'a str,
}

/// What an isolated check keeps from the server's environment: the search
/// path, the ssh agent, proxies and CA certificates. Nothing that configures git.
const ISOLATED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "SSH_AUTH_SOCK",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
];

/// The commit `git_ref` points at in the remote `url`, `None` if it has no such ref.
pub async fn ls_remote(url: &str, git_ref: &str, check: &RemoteCheck<'_>) -> anyhow::Result<Option<String>> {
    ls_remote_with(url, git_ref, check, std::env::vars_os().collect()).await
}

/// [`ls_remote`] as if the server's environment were `inherited`.
async fn ls_remote_with(
    url: &str,
    git_ref: &str,
    check: &RemoteCheck<'_>,
    inherited: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> anyhow::Result<Option<String>> {
    let mut cmd = Command::new("git");
    cmd.args(ls_remote_args(url, check.isolated)).env_clear();
    // Outside any repository, so no repository config applies either.
    let empty = check.isolated.then(tempfile::tempdir).transpose().context("creating an empty directory for git")?;
    if let Some(empty) = &empty {
        cmd.envs(inherited.into_iter().filter(|(k, _)| k.to_str().is_some_and(|k| ISOLATED_ENV.contains(&k))))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", empty.path().parent().unwrap_or(empty.path()))
            // No ssh config either: it could rename or proxy the host. Keys,
            // the agent and known_hosts work as usual; unknown hosts fail.
            .env("GIT_SSH_COMMAND", format!("{} -F /dev/null -o BatchMode=yes", shell_quote(check.ssh)))
            .env("GIT_SSH_VARIANT", "ssh")
            .current_dir(empty.path());
    } else {
        cmd.envs(inherited);
    }
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        cmd.env("GIT_ALLOW_PROTOCOL", check.protocols)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("timed out")?
    .context("running git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("git failed"));
    }
    Ok(resolve_ref(&String::from_utf8_lossy(&out.stdout), git_ref))
}

/// `git`'s arguments for [`ls_remote`]. Only an isolated check turns HTTP
/// redirects off; otherwise the server's git configuration decides.
fn ls_remote_args(url: &str, isolated: bool) -> Vec<&str> {
    let redirects: &[&str] = if isolated { &["-c", "http.followRedirects=false"] } else { &[] };
    redirects.iter().copied().chain(["ls-remote", "--", url]).collect()
}

/// `s` as one word for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The commit `git_ref` names in `ls-remote` output: tried as given, then
/// as a branch, then as a tag. For each name the peeled `<name>^{}` line wins,
/// so an annotated tag resolves to its commit however it's spelled.
fn resolve_ref(listing: &str, git_ref: &str) -> Option<String> {
    let refs: Vec<(&str, &str)> = listing.lines().filter_map(|l| l.split_once('\t')).collect();
    let find = |name: &str| refs.iter().find(|(_, n)| *n == name).map(|(sha, _)| sha.to_string());
    [git_ref.to_string(), format!("refs/heads/{git_ref}"), format!("refs/tags/{git_ref}")]
        .iter()
        .find_map(|name| find(&format!("{name}^{{}}")).or_else(|| find(name)))
}

/// `/git/*`: git smart HTTP for `repo.git`, authenticated with HTTP Basic
/// where the password is a machine (or admin) token.
pub async fn http(State(server): State<Arc<Server>>, req: Request) -> Response {
    let caller = match basic_password(req.headers()) {
        Some(token) => server.authenticate(&token).await.ok(),
        None => None,
    };
    let Some(caller) = caller else { return unauthorized() };
    let path_info = req.uri().path().strip_prefix("/git").unwrap_or_default().to_string();
    // git http-backend refuses `..` too; the data dir also holds server.db.
    if !path_info.starts_with("/repo.git/") || path_info.split('/').any(|s| s == ".." || s == ".") {
        return (StatusCode::NOT_FOUND, "only /git/repo.git is served\n").into_response();
    }
    match cgi(server, caller, path_info, req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("git http-backend: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "git backend failed\n").into_response()
        }
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"sk1ll\"")], "unauthorized\n").into_response()
}

fn basic_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    Some(text.split_once(':')?.1.to_string())
}

async fn cgi(server: Arc<Server>, caller: Caller, path_info: String, req: Request) -> anyhow::Result<Response> {
    let is_push = req.method() == "POST" && path_info.ends_with("/git-receive-pack");
    let author = caller.author().to_string();
    let receive_pack = is_push
        || (path_info.ends_with("/info/refs")
            && req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "service=git-receive-pack")));
    if receive_pack && server.push_degraded.load(Ordering::SeqCst) {
        return Ok(degraded_response());
    }
    // A push holds the lock from before receive-pack until its follow-up is
    // done, so pushes are announced one by one, in order. Removing a machine
    // takes the lock too: a push queued behind a removal of its machine is
    // refused here, and a running one finishes first.
    let push = if is_push {
        let guard = server.push_lock.clone().lock_owned().await;
        // Cleanup may have failed while this request was queued.
        if server.push_degraded.load(Ordering::SeqCst) {
            return Ok(degraded_response());
        }
        match server.db(move |conn| caller.current(conn)).await {
            Ok(_) => {}
            Err(ApiError::Unauthorized) => return Ok(unauthorized()),
            Err(e) => return Err(e.into()),
        }
        Some(guard)
    } else {
        None
    };
    let passed: Vec<(&str, String)> = [
        ("CONTENT_TYPE", header::CONTENT_TYPE),
        ("CONTENT_LENGTH", header::CONTENT_LENGTH),
        ("HTTP_CONTENT_ENCODING", header::CONTENT_ENCODING),
        ("GIT_PROTOCOL", HeaderName::from_static("git-protocol")),
    ]
    .into_iter()
    .filter_map(|(var, name)| Some((var, req.headers().get(name)?.to_str().ok()?.to_string())))
    .collect();

    let mut cmd = Command::new("git");
    cmd.arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &server.config.data_dir)
        .env("GIT_PROJECT_ROOT", &server.config.data_dir)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("REQUEST_METHOD", req.method().as_str())
        .env("PATH_INFO", &path_info)
        .env("QUERY_STRING", req.uri().query().unwrap_or_default())
        .env("REMOTE_USER", author)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // The repo's own hooks, whatever its config says: the pre-receive
        // hook there keeps pushes to the baseline branch.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", server.config.repo_dir().join("hooks"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.envs(passed);
    // receive-pack and its hooks must share the backend's lifetime, including
    // when the direct child exits before its descendants do.
    #[cfg(unix)]
    cmd.as_std_mut().process_group(0);
    let mut child = cmd.spawn().context("spawning git http-backend")?;
    #[cfg(unix)]
    let pgid = child.id().expect("newly spawned child has a pid") as libc::pid_t;

    let idle = server.config.push_idle;
    let max = server.config.push_max;
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    // This task owns the child and push guard even if the HTTP caller leaves.
    // Every exit after spawning the backend must run the push follow-up.
    tokio::spawn(async move {
        let mut stdin = child.stdin.take().expect("piped");
        let mut body = req.into_body().into_data_stream();
        let mut stdout = BufReader::new(child.stdout.take().expect("piped"));
        let mut stderr = child.stderr.take().expect("piped");
        let errors = tokio::spawn(async move {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text).await;
            if !text.trim().is_empty() {
                tracing::warn!("git http-backend: {}", text.trim());
            }
        });
        let exchange = async {
            let upload = async {
                let mut deadline = tokio::time::Instant::now() + idle;
                loop {
                    let chunk = tokio::time::timeout_at(deadline, body.next())
                        .await
                        .context("git request body inactivity timeout")?;
                    let Some(chunk) = chunk else { break };
                    let chunk = chunk.context("reading git request body")?;
                    if !chunk.is_empty() {
                        deadline = tokio::time::Instant::now() + idle;
                        tokio::time::timeout_at(deadline, stdin.write_all(&chunk))
                            .await
                            .context("git request forwarding inactivity timeout")?
                            .context("writing git request body")?;
                    }
                }
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            };
            let download = async {
                let mut builder = Response::builder().status(StatusCode::OK);
                loop {
                    let mut line = String::new();
                    if stdout.read_line(&mut line).await? == 0 {
                        bail!("git http-backend ended before its headers");
                    }
                    let line = line.trim_end_matches(['\r', '\n']);
                    if line.is_empty() {
                        break;
                    }
                    let Some((name, value)) = line.split_once(':') else { bail!("bad CGI header `{line}`") };
                    let value = value.trim();
                    if name.eq_ignore_ascii_case("status") {
                        let code = value.split_whitespace().next().unwrap_or_default();
                        builder = builder.status(StatusCode::from_bytes(code.as_bytes()).context("bad CGI status")?);
                    } else {
                        builder = builder.header(HeaderName::try_from(name)?, HeaderValue::try_from(value)?);
                    }
                }

                let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(16);
                let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) });
                if response_tx.send(builder.body(Body::from_stream(stream))?).is_err() {
                    bail!("git HTTP caller disconnected before response");
                }
                let mut chunks = ReaderStream::new(stdout);
                while let Some(chunk) = chunks.next().await {
                    let chunk = chunk.context("reading git response")?;
                    tokio::time::timeout(idle, tx.send(Ok(chunk)))
                        .await
                        .context("git response forwarding timeout")?
                        .context("git HTTP caller disconnected during response")?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let exited = async {
                let status = child.wait().await.context("waiting for git http-backend")?;
                if !status.success() {
                    tracing::warn!("git http-backend exited with {status}");
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(upload, download, exited)?;
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::time::timeout(max, exchange)
            .await
            .context("git backend overall deadline exceeded")
            .and_then(|result| result);
        if let Err(e) = result {
            tracing::warn!("git http-backend: {e:#}");
            // No descendant may update refs after follow-up examines them.
            #[cfg(unix)]
            let stopped = terminate_group(&mut child, pgid).await;
            #[cfg(not(unix))]
            let stopped = child.kill().await.map_err(anyhow::Error::from);
            // Exercise the unconfirmed-cleanup path without leaking a real
            // backend or changing process-wide state in parallel tests.
            let stopped = stopped.and_then(|()| {
                match sk1ll_core::failpoint_error!(ServerCleanupConfirmation, &server.config.repo_dir()) {
                    Some(e) => Err(anyhow::Error::from(e).context("forced cleanup confirmation failure")),
                    None => Ok(()),
                }
            });
            if let Err(e) = stopped {
                errors.abort();
                tracing::error!("git backend cleanup failed: {e:#}");
                if let Some(_guard) = push {
                    // Set the flag before releasing the guard: queued and new
                    // pushes must fail closed while a writer may survive.
                    server.push_degraded.store(true, Ordering::SeqCst);
                    tracing::error!("{DEGRADED_MESSAGE}");
                }
                return;
            }
        }
        errors.abort();
        if let Some(_guard) = push {
            server.after_push().await;
        }
    });
    response_rx.await.context("git backend ended before a response")
}

pub(crate) const DEGRADED_MESSAGE: &str =
    "server push path is degraded after an unconfirmed backend cleanup; restart the server";

fn degraded_response() -> Response {
    tracing::error!("{DEGRADED_MESSAGE}");
    (StatusCode::SERVICE_UNAVAILABLE, DEGRADED_MESSAGE).into_response()
}

#[cfg(unix)]
async fn terminate_group(child: &mut tokio::process::Child, pgid: libc::pid_t) -> anyhow::Result<()> {
    signal_group(pgid, libc::SIGTERM)?;
    if wait_for_group(child, pgid, Duration::from_millis(100)).await? {
        return Ok(());
    }
    signal_group(pgid, libc::SIGKILL)?;
    if !wait_for_group(child, pgid, Duration::from_secs(2)).await? {
        bail!("git backend process group {pgid} still exists after SIGKILL");
    }
    Ok(())
}

/// `false` means the group has already gone. Signal 0 only checks existence.
#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: pgid comes from our child in a new process group. Negating it
    // addresses that group, including receive-pack and hooks, without pointers.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) if signal == 0 => Ok(true),
        _ => Err(error),
    }
}

#[cfg(unix)]
async fn wait_for_group(child: &mut tokio::process::Child, pgid: libc::pid_t, bound: Duration) -> anyhow::Result<bool> {
    let gone = async {
        // Reap the direct child, whose zombie would otherwise keep the group
        // alive. Descendants may outlive it, so wait for ESRCH as well.
        child.wait().await.context("reaping git")?;
        while signal_group(pgid, 0)? {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    };
    match tokio::time::timeout(bound, gone).await {
        Ok(result) => result.map(|()| true),
        Err(_) => Ok(false),
    }
}
