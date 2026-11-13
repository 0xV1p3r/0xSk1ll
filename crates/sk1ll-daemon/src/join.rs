//! Joining a fleet with a single-use join code.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sk1ll_core::MachineConfig;
use sk1ll_proto::{AssignmentSnapshot, ErrorBody, JoinRequest, JoinResponse, REPO_PATH};

use crate::Config;
use crate::client::{Client, JoinApiRequest};
use crate::gitx::{self, Setup};
use crate::state::{self, DaemonState};

#[derive(Clone)]
pub struct JoinOptions {
    /// The server's base URL, e.g. `https://server.example`.
    pub server_url: String,
    pub code: String,
    /// Default: `machine.toml`'s `id`, else the hostname.
    pub machine_id: Option<String>,
    /// Start from this machine's assignment instead of everything off.
    pub copy_from: Option<String>,
    /// Where the local clone goes (e.g. `~/skills`).
    pub repo: PathBuf,
}

impl std::fmt::Debug for JoinOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinOptions")
            .field("server_url", &crate::auth::redact(&self.server_url))
            .field("code", &"<redacted>")
            .field("machine_id", &self.machine_id)
            .field("copy_from", &self.copy_from)
            .field("repo", &self.repo)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Joined {
    pub machine_id: String,
    pub repo: PathBuf,
    pub setup: Setup,
    pub harnesses: Vec<String>,
    pub revision: u64,
}

/// What the server granted, not yet stored.
pub(crate) struct Registered {
    state: DaemonState,
    snapshot: AssignmentSnapshot,
    harnesses: Vec<String>,
}

/// Joins the fleet: registers this machine and its harnesses with the server,
/// stores the token (0600) and the assignment snapshot, and clones the
/// server's repo (or adopts an existing clone of it). The repo path is checked
/// before the code is spent.
pub async fn join(cfg: &Config, opts: JoinOptions) -> anyhow::Result<Joined> {
    let reg = register(cfg, &opts, false).await?;
    reg.persist(cfg)?;
    tokio::task::spawn_blocking(move || reg.set_up_repo()).await?
}

/// [`join`], or through the running daemon if there is one (it then
/// restarts itself with the new credentials). Returns whether the daemon
/// did it. Fails rather than joining directly if a daemon may be running
/// but can't be talked to.
pub async fn join_or_rejoin(cfg: &Config, mut opts: JoinOptions) -> anyhow::Result<(Joined, bool)> {
    opts.repo = absolute(&opts.repo)?;
    let client = Client::new(&cfg.socket);
    match client.status().await {
        Ok(_) => {
            let req = JoinApiRequest {
                server_url: opts.server_url,
                code: opts.code,
                machine_id: opts.machine_id,
                copy_from: opts.copy_from,
                repo: Some(opts.repo),
            };
            let joined = client.join(&req).await.context("the running daemon couldn't join")?;
            Ok((joined, true))
        }
        Err(e) if e.is_not_running() => Ok((join(cfg, opts).await?, false)),
        Err(e) => bail!("a daemon may be running but can't be reached ({e}); stop it or try again"),
    }
}

fn absolute(p: &Path) -> anyhow::Result<PathBuf> {
    Ok(if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) })
}

/// Checks the request and spends the join code. `replace` allows replacing
/// a state that isn't revoked (the daemon decided the token is dead).
pub(crate) async fn register(cfg: &Config, opts: &JoinOptions, replace: bool) -> anyhow::Result<Registered> {
    register_with_timeout(cfg, opts, replace, std::time::Duration::from_secs(30)).await
}

async fn register_with_timeout(
    cfg: &Config,
    opts: &JoinOptions,
    replace: bool,
    timeout: std::time::Duration,
) -> anyhow::Result<Registered> {
    let server_url = opts.server_url.trim().trim_end_matches('/').to_string();
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        bail!("the server URL must start with http:// or https:// (got `{}`)", crate::auth::redact(&server_url));
    }
    if crate::state::has_credentials(&server_url) {
        bail!("the server URL must not contain credentials");
    }
    if insecure_http(&server_url) {
        tracing::warn!("joining over plain HTTP: the machine token travels in clear text");
    }
    if let Some(s) = DaemonState::load(&cfg.state)?
        && !s.revoked
        && !replace
    {
        bail!(
            "this machine already joined {} as `{}`; remove {} to join again",
            s.server_url,
            s.machine_id,
            cfg.state.display()
        );
    }
    let repo = absolute(&opts.repo)?;
    let remote = format!("{server_url}{REPO_PATH}");
    gitx::check_repo_path(&repo, &remote)?;

    let machine = MachineConfig::load(&cfg.machine_config)?;
    let machine_id = opts.machine_id.clone().unwrap_or_else(|| machine.id.clone());
    let harnesses: Vec<String> = machine.harnesses().into_keys().collect();
    let req = JoinRequest {
        code: opts.code.clone(),
        machine_id,
        harnesses: harnesses.clone(),
        copy_from: opts.copy_from.clone(),
    };
    let resp = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?
        .post(format!("{server_url}/api/join"))
        .json(&req)
        .send()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "join request failed (connection or total request timeout): {}",
                crate::auth::redact(&e.to_string())
            )
        })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let msg = resp.json::<ErrorBody>().await.map(|e| e.error).unwrap_or_else(|_| status.to_string());
        bail!("the server refused to join: {msg}");
    }
    let joined: JoinResponse = resp.json().await.context("reading the server's join response")?;
    if joined.repo_path != REPO_PATH {
        tracing::warn!("the server serves its repo at {}, not {REPO_PATH}", joined.repo_path);
    }
    Ok(Registered {
        state: DaemonState { machine_id: joined.machine_id, server_url, token: joined.token, repo, revoked: false },
        snapshot: AssignmentSnapshot { revision: joined.revision, assignment: joined.assignment },
        harnesses,
    })
}

impl Registered {
    pub(crate) fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Stores the token and the assignment snapshot. The token is shown only
    /// once, so this comes before anything else that can fail.
    pub(crate) fn persist(&self, cfg: &Config) -> anyhow::Result<()> {
        self.state.save(&cfg.state)?;
        state::save_snapshot(&cfg.snapshot_path(), &self.snapshot)?;
        // The CLI's read-only commands read the cache; it is only a mirror.
        if let Err(e) = self.snapshot.assignment.save(&cfg.assignment) {
            tracing::warn!("writing the assignment cache {}: {e}", cfg.assignment.display());
        }
        Ok(())
    }

    /// Clones (or adopts) the repo, authenticating with the new token.
    pub(crate) fn set_up_repo(&self) -> anyhow::Result<Joined> {
        let repo = &self.state.repo;
        let env = crate::auth::git_credentials_env(&self.state);
        let setup = gitx::ensure_clone(repo, &self.state.repo_url(), &env).with_context(|| {
            format!("joined, but setting up {} failed; the daemon retries at startup", repo.display())
        })?;
        // An existing clone must continue the server's history (its branch was
        // checked before the code was spent; its history needs the server).
        if setup == Setup::Adopted {
            gitx::check_shares_history(repo, &env)
                .with_context(|| format!("joined, but the existing clone at {} can't be used", repo.display()))?;
        }
        Ok(Joined {
            machine_id: self.state.machine_id.clone(),
            repo: repo.clone(),
            setup,
            harnesses: self.harnesses.clone(),
            revision: self.snapshot.revision,
        })
    }
}

/// Whether joining sends credentials over HTTP to a non-loopback host.
pub fn insecure_http(server: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(server.trim()) else { return false };
    url.scheme() == "http"
        && !url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stalled_join_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let mut cfg = Config::new().unwrap();
        cfg.state = tmp.path().join("state");
        cfg.machine_config = tmp.path().join("machine.toml");
        let opts = JoinOptions {
            server_url: url,
            code: "test".into(),
            machine_id: Some("test".into()),
            copy_from: None,
            repo: tmp.path().join("repo"),
        };
        let start = std::time::Instant::now();
        let err = register_with_timeout(&cfg, &opts, false, std::time::Duration::from_millis(100))
            .await
            .err()
            .expect("must time out");
        server.abort();
        assert!(err.to_string().contains("timeout"), "{err:#}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn http_loopback_is_exempt() {
        for url in ["http://localhost", "http://127.42.0.1", "http://[::1]", "https://example.com"] {
            assert!(!insecure_http(url), "{url}");
        }
        assert!(insecure_http("http://example.com"));
    }
}
