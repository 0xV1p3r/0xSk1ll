//! The fleet's bare repo: setup, reading the baseline, and serving git smart
//! HTTP by running `git http-backend` as a CGI program.

use std::collections::BTreeSet;
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
use sk1ll_core::skill::{IMPORTED_DIR, PERSONAL_DIR};
use sk1ll_proto::ItemRef;
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
/// `<branch>/x` through. Changing HEAD takes a restart.
pub fn configure_pushes(repo: &Path) -> anyhow::Result<String> {
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
    std::fs::write(&partial, pre_receive_hook(&branch)).context("writing the pre-receive hook")?;
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

/// A pre-receive hook that refuses every ref update but one to `branch`.
fn pre_receive_hook(branch: &str) -> String {
    let branch = shell_quote(branch);
    format!(
        "#!/bin/sh\n\
         # Written by sk1ll-server on every start; changes here are lost.\n\
         while read -r old new ref; do\n\
         \tif [ \"$ref\" != {branch} ]; then\n\
         \t\tprintf 'sk1ll-server: only %s takes pushes, not %s\\n' {branch} \"$ref\" >&2\n\
         \t\texit 1\n\
         \tfi\n\
         done\n"
    )
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

/// How long all the reads done under the push lock for one push or one
/// draft discard may take together, so the lock can't be held through many
/// consecutive [`INSPECT_TIMEOUT`]s.
const INSPECT_BUDGET: Duration = Duration::from_secs(120);

/// What is left of [`INSPECT_BUDGET`].
pub struct Budget(Duration);

impl Default for Budget {
    fn default() -> Self {
        Self(INSPECT_BUDGET)
    }
}

impl Budget {
    /// Runs `read` in what is left of the budget; `None`, logged, if it runs
    /// out. Dropping a read stops its git, children included.
    pub async fn spend<T>(&mut self, read: impl Future<Output = T>) -> Option<T> {
        if self.0.is_zero() {
            tracing::error!("repo reads used up their {INSPECT_BUDGET:?} budget");
            return None;
        }
        let start = tokio::time::Instant::now();
        let result = tokio::time::timeout(self.0, read).await;
        self.0 = self.0.saturating_sub(start.elapsed());
        if result.is_err() {
            tracing::error!("repo reads used up their {INSPECT_BUDGET:?} budget");
        }
        result.ok()
    }
}

/// Runs git in `repo`, ignoring replacement objects; `Ok(None)` if it exited
/// non-zero. An error if it outlives [`INSPECT_TIMEOUT`] or writes more than
/// [`INSPECT_OUTPUT`].
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    inspect("git".as_ref(), repo, args, INSPECT_TIMEOUT, INSPECT_OUTPUT).await
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

/// [`git`] with `program` as git, stopped (with any children) after `timeout`
/// or past `max_output` bytes of output.
async fn inspect(
    program: &std::ffi::OsStr,
    repo: &Path,
    args: &[&str],
    timeout: Duration,
    max_output: usize,
) -> anyhow::Result<Option<String>> {
    let mut cmd = Command::new(program);
    cmd.arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
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
    let run = async {
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
        let ((), status) = tokio::try_join!(read, wait)?;
        Ok::<_, anyhow::Error>((status, out))
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(result) => {
            let (status, out) = result?;
            #[cfg(unix)]
            {
                guard.0 = None;
            }
            Ok(status.success().then(|| String::from_utf8_lossy(&out).into_owned()))
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

/// Skills (`personal/<n>/SKILL.md`, `imported/<n>/SKILL.md`) and rules
/// (`instructions/<n>.md`) at `commit`.
pub async fn items_at(repo: &Path, commit: &str) -> anyhow::Result<BTreeSet<ItemRef>> {
    let args = ["ls-tree", "-r", "-z", "--name-only", commit, "--", PERSONAL_DIR, IMPORTED_DIR, INSTRUCTIONS_DIR];
    let Some(out) = git(repo, &args).await? else { bail!("cannot list tree of {commit}") };
    Ok(out.split('\0').filter_map(item_for_path).collect())
}

/// The skill or rule a tree path makes exist. Stricter than core's
/// [`ItemRef::for_path`], which maps any file under a skill dir: a skill is
/// in the baseline only with its `SKILL.md`.
fn item_for_path(path: &str) -> Option<ItemRef> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [dir, name, "SKILL.md"] if *dir == PERSONAL_DIR || *dir == IMPORTED_DIR => {
            Some(ItemRef::Skill(name.to_string()))
        }
        [dir, file] if *dir == INSTRUCTIONS_DIR => file
            .strip_suffix(".md")
            .filter(|n| !n.is_empty() && !n.starts_with('.'))
            .map(|n| ItemRef::Rule(n.to_string())),
        _ => None,
    }
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
    let receive_pack = is_push
        || (path_info.ends_with("/info/refs")
            && req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "service=git-receive-pack")));
    if receive_pack && server.push_degraded.load(Ordering::SeqCst) {
        return Ok(degraded_response());
    }
    // A push holds the lock from before receive-pack until its follow-up is
    // done, so each push is diffed and announced on its own, in order.
    // Removing a machine takes the lock too: a push queued behind a removal
    // of its machine is refused here, and a running one finishes first.
    let (push, caller) = if is_push {
        let guard = server.push_lock.clone().lock_owned().await;
        // Cleanup may have failed while this request was queued.
        if server.push_degraded.load(Ordering::SeqCst) {
            return Ok(degraded_response());
        }
        let caller = match server.db(move |conn| caller.current(conn)).await {
            Ok(caller) => caller,
            Err(ApiError::Unauthorized) => return Ok(unauthorized()),
            Err(e) => return Err(e.into()),
        };
        let mut budget = Budget::default();
        let before = budget.spend(server.read_head()).await.context("reading HEAD before a push")??;
        (Some((guard, before, budget)), caller)
    } else {
        (None, caller)
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
        .env("REMOTE_USER", caller.author())
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
            #[cfg(test)]
            let stopped = stopped.and_then(|()| {
                if server.fail_cleanup_confirmation.load(Ordering::SeqCst) {
                    bail!("forced cleanup confirmation failure");
                }
                Ok(())
            });
            if let Err(e) = stopped {
                errors.abort();
                tracing::error!("git backend cleanup failed: {e:#}");
                if let Some((_guard, ..)) = push {
                    // Set the flag before releasing the guard. Queued and new
                    // pushes must fail closed while a writer may survive,
                    // and nothing may trust the HEAD read before it.
                    server.push_degraded.store(true, Ordering::SeqCst);
                    server.forget_head();
                    tracing::error!("{DEGRADED_MESSAGE}");
                }
                return;
            }
        }
        errors.abort();
        if let Some((_guard, before, budget)) = push {
            server.after_push(before, budget, &caller).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(uri: &str, method: &str, body: Body) -> Request {
        Request::builder()
            .uri(uri)
            .method(method)
            .header(header::AUTHORIZATION, "Basic YWRtaW46dGVzdA==") // admin:test
            .header(header::CONTENT_TYPE, "application/x-git-receive-pack-request")
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn unconfirmed_cleanup_rejects_queued_and_new_pushes_but_allows_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::new(dir.path().join("data"));
        config.admin_token = Some("test".into());
        config.push_idle = Duration::from_millis(200);
        let server = Server::open(config).await.unwrap();
        server.fail_cleanup_confirmation.store(true, Ordering::SeqCst);
        let push_path = "/repo.git/git-receive-pack";
        let stalled = tokio::spawn({
            let server = server.clone();
            async move {
                let body = Body::from_stream(futures_util::stream::pending::<std::io::Result<Bytes>>());
                cgi(server, Caller::Admin, push_path.into(), request("/git/repo.git/git-receive-pack", "POST", body))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.push_lock.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stalled push never acquired the lock");
        let mut queued = Box::pin(cgi(
            server.clone(),
            Caller::Admin,
            push_path.into(),
            request("/git/repo.git/git-receive-pack", "POST", Body::empty()),
        ));
        assert!(futures_util::poll!(queued.as_mut()).is_pending(), "second push must queue behind the first");
        // The first request may already have sent headers before timing out.
        let _ = tokio::time::timeout(Duration::from_secs(4), stalled).await.unwrap().unwrap();
        let response = tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .expect("queued push hung after cleanup failed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, DEGRADED_MESSAGE);
        assert!(server.push_lock.try_lock().is_ok(), "degraded state must not leak the push guard");
        for (uri, method) in
            [("/git/repo.git/git-receive-pack", "POST"), ("/git/repo.git/info/refs?service=git-receive-pack", "GET")]
        {
            let response = tokio::time::timeout(
                Duration::from_secs(1),
                http(State(server.clone()), request(uri, method, Body::empty())),
            )
            .await
            .expect("degraded receive-pack request hung");
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(body, DEGRADED_MESSAGE);
        }
        let health = crate::healthz(State(server.clone())).await;
        assert_eq!(health.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(health.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "status": "degraded", "push_degraded": true,
                "version": sk1ll_core::VERSION, "protocol": sk1ll_proto::PROTOCOL_VERSION,
            })
        );
        // A real clone exercises both upload-pack discovery and its POST.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = server.router();
        let serve_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let clone = dir.path().join("clone");
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new("git")
                .args(["clone", "--quiet", &format!("http://admin:test@{address}/git/repo.git")])
                .arg(&clone)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("fetch hung while pushes were degraded")
        .unwrap();
        serve_task.abort();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(clone.join("sk1ll.toml").is_file());
        // The degraded state lasts only for this process, not across restart.
        let restarted = Server::open(server.config.clone()).await.unwrap();
        assert!(!restarted.push_degraded.load(Ordering::SeqCst));
    }

    /// A server with machine `m` (token `t`) and that machine as a caller.
    async fn with_machine() -> (tempfile::TempDir, Arc<Server>, Caller) {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::open(crate::Config::new(dir.path().join("data"))).await.unwrap();
        let (a, scope) = (sk1ll_proto::Assignment::default(), sk1ll_proto::Scope::Fleet);
        server.db(move |conn| crate::db::insert_machine(conn, "m", &crate::hash("t"), 0, &a, scope)).await.unwrap();
        let caller = Caller::Machine { id: "m".into(), scope, token_hash: crate::hash("t") };
        (dir, server, caller)
    }

    fn machine_exists(server: &Arc<Server>) -> impl Future<Output = bool> {
        let server = server.clone();
        async move { server.db(|conn| Ok(crate::db::machine(conn, "m")?.is_some())).await.unwrap() }
    }

    /// Review finding: a push queued behind its machine's removal must be
    /// refused before receive-pack starts, not run with the removed token.
    #[tokio::test]
    async fn a_push_queued_behind_its_machines_removal_is_refused() {
        let (_dir, server, caller) = with_machine().await;
        let held = server.push_lock.clone().lock_owned().await;
        let mut removal = Box::pin(server.remove_machine(Caller::Admin, "m".into()));
        assert!(futures_util::poll!(removal.as_mut()).is_pending(), "the removal waits for the push lock");
        let push_path = "/repo.git/git-receive-pack";
        let mut push = Box::pin(cgi(
            server.clone(),
            caller,
            push_path.into(),
            request("/git/repo.git/git-receive-pack", "POST", Body::empty()),
        ));
        assert!(futures_util::poll!(push.as_mut()).is_pending(), "the push queues behind the removal");
        drop(held);
        tokio::time::timeout(Duration::from_secs(2), removal).await.unwrap().unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), push).await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(server.push_lock.try_lock().is_ok());
    }

    /// The other order: a removal requested while its machine's push runs
    /// waits until the push, follow-up included, is done.
    #[tokio::test]
    async fn a_removal_waits_for_its_machines_running_push() {
        let (_dir, server, caller) = with_machine().await;
        let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(1);
        let body =
            Body::from_stream(futures_util::stream::unfold(
                rx,
                |mut rx| async move { rx.recv().await.map(|c| (c, rx)) },
            ));
        let push_path = "/repo.git/git-receive-pack";
        let push = tokio::spawn(cgi(server.clone(), caller, push_path.into(), request(push_path, "POST", body)));
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.push_lock.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the push never acquired the lock");
        let removal = tokio::spawn({
            let server = server.clone();
            async move { server.remove_machine(Caller::Admin, "m".into()).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!removal.is_finished() && machine_exists(&server).await, "the removal must wait for the push");
        // An empty command list: receive-pack accepts it and changes nothing.
        tx.send(Ok(Bytes::from_static(b"0000"))).await.unwrap();
        drop(tx);
        let response = tokio::time::timeout(Duration::from_secs(5), push).await.unwrap().unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        tokio::time::timeout(Duration::from_secs(5), removal).await.unwrap().unwrap().unwrap();
        assert!(!machine_exists(&server).await);
    }

    /// Review finding: the removal's caller going away (the future dropped
    /// while the database work runs) must not let a queued push take the lock
    /// and pass its token check before the removal commits.
    #[tokio::test]
    async fn a_dropped_removal_keeps_queued_pushes_out_until_it_commits() {
        let (_dir, server, caller) = with_machine().await;
        let held = server.db.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _guard = crate::lock(&held);
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let mut removal = Box::pin(server.remove_machine(Caller::Admin, "m".into()));
        assert!(futures_util::poll!(removal.as_mut()).is_pending(), "the removal waits for the database");
        drop(removal);
        assert!(server.push_lock.try_lock().is_err(), "the removal's database work still holds the push lock");
        let push_path = "/repo.git/git-receive-pack";
        let mut push = Box::pin(cgi(
            server.clone(),
            caller,
            push_path.into(),
            request("/git/repo.git/git-receive-pack", "POST", Body::empty()),
        ));
        assert!(futures_util::poll!(push.as_mut()).is_pending(), "the push queues behind the removal");
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        let response = tokio::time::timeout(Duration::from_secs(2), push).await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!machine_exists(&server).await);
    }

    /// Runs git like the server's bootstrap does; its trimmed stdout.
    fn run(dir: &Path, args: &[&str]) -> String {
        let out = setup_git().arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Review finding: a replacement ref already in the repo (pushed before
    /// pushes to it were refused, or made by hand) must not change what the
    /// server lists, even after something unset the repo's own setting.
    #[tokio::test]
    async fn replacement_refs_do_not_change_what_the_server_reads() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::Config::new(dir.path().join("data"));
        let repo = config.repo_dir();
        drop(Server::open(config.clone()).await.unwrap());
        let work = dir.path().join("work");
        run(dir.path(), &["clone", "--quiet", &repo.to_string_lossy(), &work.to_string_lossy()]);
        std::fs::create_dir_all(work.join("personal/pdf")).unwrap();
        std::fs::write(work.join("personal/pdf/SKILL.md"), "x\n").unwrap();
        run(&work, &["add", "--all"]);
        run(&work, &["commit", "--quiet", "-m", "pdf"]);
        run(&work, &["push", "--quiet", "origin", "HEAD:main"]);
        let with = run(&work, &["rev-parse", "HEAD"]);
        run(&work, &["rm", "--quiet", "-r", "personal/pdf"]);
        run(&work, &["commit", "--quiet", "-m", "without pdf"]);
        let without = run(&work, &["rev-parse", "HEAD"]);
        run(&repo, &["fetch", "--quiet", &work.to_string_lossy(), "HEAD:refs/heads/tmp"]);
        run(&repo, &["replace", &with, &without]);
        run(&repo, &["update-ref", "-d", "refs/heads/tmp"]);
        run(&repo, &["config", "--unset", "core.useReplaceRefs"]);
        // Plain git honours the replacement, so the setup is sound.
        let plain = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&plain.stdout).contains("personal/pdf"));

        let server = Server::open(config).await.unwrap();
        let pdf = ItemRef::Skill("pdf".into());
        assert_eq!(server.head().as_deref(), Some(with.as_str()));
        assert!(server.baseline_items().contains(&pdf), "the baseline listing ignores the replacement");
        assert!(items_at(&repo, &with).await.unwrap().contains(&pdf));
        assert_eq!(run(&repo, &["config", "core.useReplaceRefs"]), "false", "startup sets it again");
    }

    /// Review finding: reads with the push lock held must have a deadline,
    /// and a read that misses it is stopped, children included.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stuck_git_read_times_out_and_is_stopped_with_its_children() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stub = dir.path().join("git");
        let script =
            format!("#!/bin/sh\nsh -c 'trap \"\" TERM; exec sleep 30' &\necho $! > '{}'\nwait\n", pid_file.display());
        std::fs::write(&stub, script).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = std::time::Instant::now();
        let err =
            inspect(stub.as_os_str(), dir.path(), &["rev-parse", "HEAD"], Duration::from_millis(300), INSPECT_OUTPUT)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        let pid: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        // SAFETY: signal 0 only checks whether the process exists.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "the stub's TERM-resistant child must be gone");
    }

    /// A stub git that starts a TERM-resistant child, records its pid in
    /// `pid_file`, then runs `then`.
    #[cfg(unix)]
    fn stub_git(dir: &Path, pid_file: &Path, then: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let stub = dir.join("git");
        let script =
            format!("#!/bin/sh\nsh -c 'trap \"\" TERM; exec sleep 30' &\necho $! > '{}'\n{then}\n", pid_file.display());
        std::fs::write(&stub, script).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    #[cfg(unix)]
    fn running(pid_file: &Path) -> bool {
        let pid: libc::pid_t = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
        // SAFETY: signal 0 only checks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Review finding: a read's output is capped, and going past the cap
    /// fails the read and stops its git, children included.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_past_its_output_cap_fails_and_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stub = stub_git(dir.path(), &pid_file, "exec yes");
        let err =
            inspect(stub.as_os_str(), dir.path(), &["ls-tree"], Duration::from_secs(10), 1 << 20).await.unwrap_err();
        assert!(err.to_string().contains("wrote more than"), "{err}");
        tokio::time::timeout(Duration::from_secs(2), async {
            while running(&pid_file) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stub's TERM-resistant child must be gone");
    }

    /// Review finding: a read dropped before its deadline (its caller
    /// cancelled, or a budget ran out) stops its whole process group.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_read_stops_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let stub = stub_git(dir.path(), &pid_file, "wait");
        let mut budget = Budget(Duration::from_millis(300));
        let read = inspect(stub.as_os_str(), dir.path(), &["rev-parse"], Duration::from_secs(30), INSPECT_OUTPUT);
        assert!(budget.spend(read).await.is_none(), "the budget runs out first");
        assert_eq!(budget.0, Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(2), async {
            while running(&pid_file) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stub's TERM-resistant child must be gone");
        assert!(budget.spend(async {}).await.is_none(), "a spent budget runs nothing more");
    }

    #[test]
    fn redirects_are_turned_off_only_for_isolated_checks() {
        let url = "https://example.com/o/r.git";
        assert_eq!(ls_remote_args(url, false), ["ls-remote", "--", url], "the server's git config decides");
        assert_eq!(ls_remote_args(url, true), ["-c", "http.followRedirects=false", "ls-remote", "--", url]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_kills_term_resistant_descendant_after_backend_exits() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("hook-ready");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", r#"sh -c 'trap "" TERM; touch "$1"; exec sleep 30' hook "$1" &"#, "backend"])
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd.as_std_mut().process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pgid = child.id().unwrap() as libc::pid_t;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(child.wait().await.unwrap().success());
        })
        .await
        .expect("the backend must exit while its TERM-resistant descendant remains");
        assert!(signal_group(pgid, 0).unwrap(), "the descendant should still be running");
        terminate_group(&mut child, pgid).await.unwrap();
        assert!(!signal_group(pgid, 0).unwrap(), "cleanup must wait until the entire group is gone");
    }

    #[test]
    fn maps_paths_to_items() {
        assert_eq!(item_for_path("personal/pdf/SKILL.md"), Some(ItemRef::Skill("pdf".into())));
        assert_eq!(item_for_path("imported/x/SKILL.md"), Some(ItemRef::Skill("x".into())));
        assert_eq!(item_for_path("instructions/10-git.md"), Some(ItemRef::Rule("10-git".into())));
        assert_eq!(item_for_path("personal/pdf/ref/SKILL.md"), None);
        assert_eq!(item_for_path("instructions/notes.txt"), None);
        assert_eq!(item_for_path("instructions/sub/a.md"), None);
        assert_eq!(item_for_path("instructions/.md"), None);
        assert_eq!(item_for_path("instructions/.hidden.md"), None);
    }

    #[test]
    fn annotated_tags_resolve_to_their_commit() {
        let listing = "c0ffee\tHEAD\n\
                       aaaa\trefs/heads/main\n\
                       1111\trefs/tags/v1\n\
                       2222\trefs/tags/v1^{}\n\
                       3333\trefs/tags/light\n";
        assert_eq!(resolve_ref(listing, "v1").as_deref(), Some("2222"));
        assert_eq!(resolve_ref(listing, "refs/tags/v1").as_deref(), Some("2222"));
        assert_eq!(resolve_ref(listing, "light").as_deref(), Some("3333"));
        assert_eq!(resolve_ref(listing, "main").as_deref(), Some("aaaa"));
        assert_eq!(resolve_ref(listing, "refs/heads/main").as_deref(), Some("aaaa"));
        assert_eq!(resolve_ref(listing, "HEAD").as_deref(), Some("c0ffee"));
        assert_eq!(resolve_ref(listing, "nope"), None);
    }
}

#[cfg(test)]
mod transport_tests {
    use super::{RemoteCheck, ls_remote, ls_remote_with};
    use crate::config::IMPORT_PROTOCOLS;

    const OPEN: RemoteCheck = RemoteCheck { protocols: IMPORT_PROTOCOLS, isolated: false, ssh: "ssh" };

    /// An `ssh` that logs its arguments to `log` and fails.
    #[cfg(unix)]
    fn recording_ssh(dir: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n", log.display())).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        ssh
    }

    /// Review finding: with an allowlist, url rewrites from the server's git
    /// config (global file or `GIT_CONFIG_*` variables), its ssh command and
    /// its ssh config must not send an approved URL to another host.
    #[cfg(unix)]
    #[tokio::test]
    async fn isolated_checks_ignore_the_servers_git_and_ssh_config() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("ssh.log");
        let ssh = recording_ssh(dir.path(), &log);
        let rewrite = dir.path().join("gitconfig");
        std::fs::write(&rewrite, "[url \"ssh://git@evil.example/\"]\n\tinsteadOf = ssh://git@github.com/\n").unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let ssh_command = ("GIT_SSH_COMMAND".into(), ssh.as_os_str().to_owned());
        let inherited: [Vec<(std::ffi::OsString, std::ffi::OsString)>; 2] = [
            vec![("PATH".into(), path.clone()), ("GIT_CONFIG_GLOBAL".into(), rewrite.into()), ssh_command.clone()],
            vec![
                ("PATH".into(), path),
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                ("GIT_CONFIG_KEY_0".into(), "url.ssh://git@evil.example/.insteadOf".into()),
                ("GIT_CONFIG_VALUE_0".into(), "ssh://git@github.com/".into()),
                ssh_command,
            ],
        ];
        let url = "ssh://git@github.com/o/r.git";
        let ssh = ssh.to_str().unwrap();
        for env in inherited {
            // Without isolation the rewrite applies, so the setup is sound.
            std::fs::remove_file(&log).ok();
            assert!(ls_remote_with(url, "main", &OPEN, env.clone()).await.is_err());
            let seen = std::fs::read_to_string(&log).unwrap();
            assert!(seen.contains("git@evil.example"), "{seen}");

            std::fs::remove_file(&log).ok();
            let check = RemoteCheck { isolated: true, ssh, ..OPEN };
            assert!(ls_remote_with(url, "main", &check, env).await.is_err());
            let seen = std::fs::read_to_string(&log).unwrap();
            assert!(!seen.contains("evil") && seen.contains("git@github.com"), "{seen}");
            assert!(seen.starts_with("-F /dev/null -o BatchMode=yes "), "{seen}");
        }
    }

    #[tokio::test]
    async fn import_transports_are_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let url = format!("ext::sh -c touch% {}", marker.display());
        let err = ls_remote(&url, "main", &OPEN).await.unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        assert!(!marker.exists());
        let mut init = std::process::Command::new("git");
        init.arg("-C").arg(dir.path()).args(["init", "--bare", "--quiet"]);
        assert!(init.status().unwrap().success());
        for url in [dir.path().display().to_string(), format!("file://{}", dir.path().display())] {
            let err = ls_remote(&url, "main", &OPEN).await.unwrap_err();
            assert!(err.to_string().contains("transport 'file' not allowed"), "{err}");
        }
    }
}
