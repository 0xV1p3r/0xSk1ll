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
    /// The hub's base URL, e.g. `https://hub.example`.
    pub hub_url: String,
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
            .field("hub_url", &crate::auth::redact(&self.hub_url))
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

/// What the hub granted, not yet stored.
pub(crate) struct Registered {
    state: DaemonState,
    snapshot: AssignmentSnapshot,
    harnesses: Vec<String>,
}

/// Joins the fleet: registers this machine and its harnesses with the hub,
/// stores the token (0600) and the assignment snapshot, and clones the
/// hub's repo (or adopts an existing clone of it). The repo path is checked
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
                hub_url: opts.hub_url,
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
    let hub_url = opts.hub_url.trim().trim_end_matches('/').to_string();
    if !(hub_url.starts_with("http://") || hub_url.starts_with("https://")) {
        bail!("the hub URL must start with http:// or https:// (got `{}`)", crate::auth::redact(&hub_url));
    }
    if crate::state::has_credentials(&hub_url) {
        bail!("the hub URL must not contain credentials");
    }
    if let Some(s) = DaemonState::load(&cfg.state)?
        && !s.revoked
        && !replace
    {
        bail!(
            "this machine already joined {} as `{}`; remove {} to join again",
            s.hub_url,
            s.machine_id,
            cfg.state.display()
        );
    }
    let repo = absolute(&opts.repo)?;
    let remote = format!("{hub_url}{REPO_PATH}");
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
    let resp = reqwest::Client::new()
        .post(format!("{hub_url}/api/join"))
        .json(&req)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach the hub at {hub_url}: {}", crate::auth::redact(&e.to_string())))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let msg = resp.json::<ErrorBody>().await.map(|e| e.error).unwrap_or_else(|_| status.to_string());
        bail!("the hub refused to join: {msg}");
    }
    let joined: JoinResponse = resp.json().await.context("reading the hub's join response")?;
    if joined.repo_path != REPO_PATH {
        tracing::warn!("the hub serves its repo at {}, not {REPO_PATH}", joined.repo_path);
    }
    Ok(Registered {
        state: DaemonState {
            machine_id: joined.machine_id,
            hub_url,
            token: joined.token,
            repo,
            revoked: false,
        },
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
        // An existing clone must continue the hub's history (its branch was
        // checked before the code was spent; its history needs the hub).
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
