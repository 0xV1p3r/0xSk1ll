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

use crate::Server;

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
    std::fs::create_dir_all(repo.join("hooks")).context("creating bare repo hooks directory")?;
    if created {
        tracing::info!("created {}", repo.display());
    }
    Ok(())
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

/// Runs git in `repo`; `Ok(None)` if it exited non-zero.
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git")?;
    Ok(out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// The baseline: HEAD's commit, `None` while the repo is empty.
pub async fn head(repo: &Path) -> anyhow::Result<Option<String>> {
    Ok(git(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).await?.map(|s| s.trim().to_string()))
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

/// The commit `git_ref` points at in the remote `url`, `None` if it has no such ref.
/// Only the transports in `protocols` (git's `GIT_ALLOW_PROTOCOL` list) are
/// used: the URL comes from a pushed manifest, so `ext::` and local paths
/// must not reach git.
pub async fn ls_remote(url: &str, git_ref: &str, protocols: &str) -> anyhow::Result<Option<String>> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new("git")
            .args(["ls-remote", "--", url])
            .env("GIT_ALLOW_PROTOCOL", protocols)
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
    let Some(caller) = caller else {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"sk1ll\"")], "unauthorized\n")
            .into_response();
    };
    let path_info = req.uri().path().strip_prefix("/git").unwrap_or_default().to_string();
    if !path_info.starts_with("/repo.git/") {
        return (StatusCode::NOT_FOUND, "only /git/repo.git is served\n").into_response();
    }
    match cgi(server, caller.author().to_string(), path_info, req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("git http-backend: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, "git backend failed\n").into_response()
        }
    }
}

fn basic_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    Some(text.split_once(':')?.1.to_string())
}

async fn cgi(server: Arc<Server>, user: String, path_info: String, req: Request) -> anyhow::Result<Response> {
    let is_push = req.method() == "POST" && path_info.ends_with("/git-receive-pack");
    let receive_pack = is_push
        || (path_info.ends_with("/info/refs")
            && req.uri().query().is_some_and(|q| q.split('&').any(|p| p == "service=git-receive-pack")));
    if receive_pack && server.push_degraded.load(Ordering::SeqCst) {
        return Ok(degraded_response());
    }
    // A push holds the lock from before receive-pack until its follow-up is
    // done, so each push is diffed and announced on its own, in order.
    let push = if is_push {
        let guard = server.push_lock.clone().lock_owned().await;
        // Cleanup may have failed while this request was queued.
        if server.push_degraded.load(Ordering::SeqCst) {
            return Ok(degraded_response());
        }
        Some((guard, head(&server.config.repo_dir()).await?))
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
        .env("REMOTE_USER", &user)
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
            let stopped = terminate_backend_group(&mut child, pgid).await;
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
                if let Some((_guard, _)) = push {
                    // Set the flag before releasing the guard. Queued and new
                    // pushes must fail closed while a writer may survive.
                    server.push_degraded.store(true, Ordering::SeqCst);
                    tracing::error!("{DEGRADED_MESSAGE}");
                }
                return;
            }
        }
        errors.abort();
        if let Some((_guard, before)) = push {
            server.after_push(before).await;
        }
    });
    response_rx.await.context("git backend ended before a response")
}

const DEGRADED_MESSAGE: &str = "server push path is degraded after an unconfirmed backend cleanup; restart the server";

fn degraded_response() -> Response {
    tracing::error!("{DEGRADED_MESSAGE}");
    (StatusCode::SERVICE_UNAVAILABLE, DEGRADED_MESSAGE).into_response()
}

#[cfg(unix)]
async fn terminate_backend_group(child: &mut tokio::process::Child, pgid: libc::pid_t) -> anyhow::Result<()> {
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
        child.wait().await.context("reaping git http-backend")?;
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
                cgi(server, "admin".into(), push_path.into(), request("/git/repo.git/git-receive-pack", "POST", body))
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
            "admin".into(),
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
        terminate_backend_group(&mut child, pgid).await.unwrap();
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
    use super::ls_remote;
    use crate::config::IMPORT_PROTOCOLS;

    #[tokio::test]
    async fn import_transports_are_restricted() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let url = format!("ext::sh -c touch% {}", marker.display());
        let err = ls_remote(&url, "main", IMPORT_PROTOCOLS).await.unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        assert!(!marker.exists());
        let mut init = std::process::Command::new("git");
        init.arg("-C").arg(dir.path()).args(["init", "--bare", "--quiet"]);
        assert!(init.status().unwrap().success());
        for url in [dir.path().display().to_string(), format!("file://{}", dir.path().display())] {
            let err = ls_remote(&url, "main", IMPORT_PROTOCOLS).await.unwrap_err();
            assert!(err.to_string().contains("transport 'file' not allowed"), "{err}");
        }
    }
}
