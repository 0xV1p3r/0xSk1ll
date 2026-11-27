//! Joining a fleet with a single-use join code.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sk1ll_core::MachineConfig;
use sk1ll_proto::{AssignmentSnapshot, ErrorBody, JoinCode, JoinRequest, JoinResponse, REPO_PATH};

use crate::Config;
use crate::client::{Client, JoinApiRequest};
use crate::state::{self, DaemonState};
use sk1ll_core::git::plumbing::{self, Setup};

#[derive(Clone)]
pub struct JoinOptions {
    /// The server's base URL, e.g. `https://server.example`.
    pub server_url: String,
    /// Empty to create one with `admin_token`.
    pub code: String,
    /// The server's admin token, used once to create a join code for the
    /// first machine of a new fleet. Never stored.
    pub admin_token: Option<String>,
    /// Default: `machine.toml`'s `id`, else the hostname.
    pub machine_id: Option<String>,
    /// Start from this machine's assignment instead of everything off.
    pub copy_from: Option<String>,
    /// Where the local clone goes (e.g. `~/.local/share/sk1ll/repo`).
    pub repo: PathBuf,
}

impl std::fmt::Debug for JoinOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinOptions")
            .field("server_url", &crate::auth::redact(&self.server_url))
            .field("code", &"<redacted>")
            .field("admin_token", &self.admin_token.as_ref().map(|_| "<redacted>"))
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

/// The harnesses a Machine reports to the Server: names and kinds.
pub(crate) fn reported_harnesses(
    harnesses: &std::collections::BTreeMap<String, sk1ll_core::Harness>,
) -> Vec<sk1ll_proto::ReportedHarness> {
    harnesses
        .iter()
        .map(|(name, h)| sk1ll_proto::ReportedHarness { name: name.clone(), kind: h.kind.clone() })
        .collect()
}

/// What the server granted, not yet stored.
#[derive(Serialize, Deserialize)]
pub(crate) struct Registered {
    state: DaemonState,
    snapshot: AssignmentSnapshot,
    harnesses: Vec<String>,
}

/// Joins the fleet: registers this machine and its harnesses with the server,
/// stores the token (0600) and the assignment snapshot, and clones the
/// server's repo (or adopts an existing clone of it). The repo path is checked
/// before the code is spent. Holds the machine lock throughout, so no engine
/// runs on the Machine meanwhile.
pub async fn join(cfg: &Config, opts: JoinOptions) -> anyhow::Result<Joined> {
    let _machine = crate::MachineLock::acquire(cfg)?;
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
                admin_token: opts.admin_token,
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

/// Asks the server for a single-use join code with its admin token.
async fn create_join_code(http: &reqwest::Client, server_url: &str, admin_token: &str) -> anyhow::Result<String> {
    let resp = http
        .post(format!("{server_url}/api/join-codes"))
        .bearer_auth(admin_token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("join code request failed: {}", crate::auth::redact(&e.to_string())))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED || resp.status() == reqwest::StatusCode::FORBIDDEN {
        bail!("the server rejected the admin token");
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let msg = resp.json::<ErrorBody>().await.map(|e| e.error).unwrap_or_else(|_| status.to_string());
        bail!("the server refused to create a join code: {msg}");
    }
    Ok(resp.json::<JoinCode>().await.context("reading the server's join code")?.code)
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
    register_inner(cfg, opts, replace, timeout, false).await
}

pub(crate) async fn register_standalone(cfg: &Config, opts: &JoinOptions) -> anyhow::Result<(Registered, PathBuf)> {
    let url = opts.server_url.trim().trim_end_matches('/');
    let repo = absolute(&opts.repo)?;
    plumbing::check_standalone_join(&repo, &format!("{url}{REPO_PATH}"))?;
    let machine = MachineConfig::load(&cfg.machine_config)?;
    let id = opts.machine_id.as_deref().unwrap_or(&machine.id);
    // Repeating the original request after a local failure uses the issued
    // token. A consumed join code must never be sent to the server again.
    if let Some(parent) = cfg.state.parent() {
        match std::fs::read_dir(parent) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    if !name.to_string_lossy().starts_with("daemon-pending-join-") || !entry.file_type()?.is_file() {
                        continue;
                    }
                    let reg: Registered = serde_json::from_slice(&std::fs::read(entry.path())?)
                        .with_context(|| format!("reading pending registration {}", entry.path().display()))?;
                    if reg.state.server_url == url && reg.state.repo == repo && reg.state.machine_id == id {
                        return Ok((reg, entry.path()));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let reg = register_inner(cfg, opts, false, std::time::Duration::from_secs(30), true).await?;
    let pending = reg.persist_pending(cfg)?;
    Ok((reg, pending))
}

async fn register_inner(
    cfg: &Config,
    opts: &JoinOptions,
    replace: bool,
    timeout: std::time::Duration,
    standalone: bool,
) -> anyhow::Result<Registered> {
    let server_url = opts.server_url.trim().trim_end_matches('/').to_string();
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        bail!("the server URL must start with http:// or https:// (got `{}`)", crate::auth::redact(&server_url));
    }
    if plumbing::has_credentials(&server_url) {
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
    if standalone {
        plumbing::check_standalone_join(&repo, &remote)?;
    } else {
        plumbing::check_repo_path(&repo, &remote)?;
    }

    // Joining a machine without a repo is its first-run setup: it takes up
    // the opt-in presets setup kept on before registering its harnesses.
    if !repo.join(".git").exists() {
        for warning in MachineConfig::adopt_detected_presets(&cfg.machine_config, &repo)? {
            tracing::warn!("{warning}");
        }
    }
    let machine = MachineConfig::load(&cfg.machine_config)?;
    let machine_id = opts.machine_id.clone().unwrap_or_else(|| machine.id.clone());
    let reported = reported_harnesses(&machine.harnesses());
    let harnesses: Vec<String> = reported.iter().map(|h| h.name.clone()).collect();
    let http =
        crate::server::http_client().timeout(timeout).connect_timeout(std::time::Duration::from_secs(15)).build()?;
    // Created only now, after the local checks, so a refused join doesn't
    // leave an unused code behind.
    // A given code wins, so an exported SK1LL_SERVER_ADMIN_TOKEN doesn't get in the way.
    let code = match (opts.code.trim(), opts.admin_token.as_deref().map(str::trim)) {
        ("", Some(token)) if !token.is_empty() => create_join_code(&http, &server_url, token).await?,
        ("", _) => bail!("a join code or the server's admin token is needed"),
        (code, _) => code.to_string(),
    };
    let req = JoinRequest { code, machine_id, harnesses: reported, copy_from: opts.copy_from.clone() };
    let resp = http.post(format!("{server_url}/api/join")).json(&req).send().await.map_err(|e| {
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
        state: DaemonState::new(joined.machine_id, server_url, joined.token, repo),
        snapshot: AssignmentSnapshot { revision: joined.revision, assignment: joined.assignment, registration: None },
        harnesses,
    })
}

impl Registered {
    pub(crate) fn state(&self) -> &DaemonState {
        &self.state
    }

    /// Retain issued credentials without activating joined mode. Each attempt
    /// gets its own private file so a refused attempt cannot erase an older one.
    pub(crate) fn persist_pending(&self, cfg: &Config) -> anyhow::Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::DirBuilderExt;
        let parent = cfg.state.parent().context("state path has no parent")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let mut file = tempfile::Builder::new().prefix("daemon-pending-join-").suffix(".json").tempfile_in(parent)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        let (_, path) = file.keep()?;
        tracing::info!("kept pending fleet registration at {}", path.display());
        Ok(path)
    }

    /// Stores the assignment snapshot, then the token. The token is shown
    /// only once, so this comes before anything else that can fail. The
    /// snapshot is tagged with the new registration and written first: a
    /// stop in between leaves the old credentials, which ignore it, never the
    /// new credentials with the old registration's snapshot.
    pub(crate) fn persist(&self, cfg: &Config) -> anyhow::Result<()> {
        state::save_snapshot(&cfg.snapshot_path(), &self.snapshot, Some(&self.state.registration_key()))?;
        if let Some(e) = sk1ll_core::failpoint_error!(DaemonJoinAfterSnapshot, &cfg.state) {
            return Err(e.into());
        }
        self.state.save(&cfg.state)?;
        // The CLI's read-only commands read the cache; it is only a mirror.
        if let Err(e) = self.snapshot.assignment.save(&cfg.assignment) {
            tracing::warn!("writing the assignment cache {}: {e}", cfg.assignment.display());
        }
        Ok(())
    }

    /// A registration that is not used after all: best effort, asks the
    /// server to remove it again with its own token. If the server can't be
    /// reached, it stays in the Fleet as a Machine that never connects,
    /// for an admin to remove.
    pub(crate) async fn discard(&self, http: &reqwest::Client) {
        let url = format!("{}/api/machines/{}", self.state.server_url, self.state.machine_id);
        let sent = http.delete(url).bearer_auth(&self.state.token).timeout(std::time::Duration::from_secs(10)).send();
        match sent.await.map(|r| r.error_for_status()) {
            Ok(Ok(_)) => tracing::info!("removed the unused registration `{}` again", self.state.machine_id),
            Ok(Err(e)) | Err(e) => tracing::warn!(
                "the unused registration `{}` stays in the Fleet until it is removed: {}",
                self.state.machine_id,
                crate::auth::redact(&e.to_string())
            ),
        }
    }

    /// Build the replacement privately and lock it before it can become live.
    pub(crate) fn prepare_standalone_repo(&self) -> anyhow::Result<StandaloneRepo> {
        let repo = &self.state.repo;
        plumbing::check_standalone_join(repo, &self.state.repo_url())?;
        let pristine = plumbing::is_pristine_init(repo)?;
        let exists = repo.symlink_metadata().is_ok();
        let empty = exists && std::fs::read_dir(repo)?.next().is_none();
        if !pristine && exists && !empty {
            // An unchanged clone of this fleet stays in place and keeps the
            // engine's existing lock. No local content is replaced.
            return Ok(StandaloneRepo { joined: self.set_up_repo()?, replacement: None });
        }
        let parent = repo.parent().context("repo has no parent")?;
        std::fs::create_dir_all(parent)?;
        let area = tempfile::Builder::new().prefix("sk1ll-before-join-").tempdir_in(parent)?.keep();
        let stage = area.join("replacement");
        let joined = self.set_up_repo_at(&stage)?;
        let lock = sk1ll_core::EngineLock::acquire(&stage)?;
        Ok(StandaloneRepo {
            joined,
            replacement: Some(Replacement {
                repo: repo.clone(),
                area,
                lock: Some(lock),
                had_original: exists,
                moved_original: false,
                published: false,
            }),
        })
    }

    /// The credentials are the activation marker. Publish them only after
    /// snapshot and mirror succeed, restoring every earlier file on failure.
    pub(crate) fn persist_standalone(&self, cfg: &Config) -> anyhow::Result<()> {
        let mut files = [
            ActivationFile::stage(
                cfg.snapshot_path(),
                &state::StoredSnapshot::new(&self.snapshot, Some(&self.state.registration_key())),
                true,
            )?,
            ActivationFile::stage(cfg.assignment.clone(), &self.snapshot.assignment, true)?,
            ActivationFile::stage(cfg.state.clone(), &self.state, false)?,
        ];
        for i in 0..files.len() {
            if i == files.len() - 1 {
                sk1ll_core::failpoint!(DaemonJoinBeforeCredentials, &cfg.state);
            }
            if let Err(error) = files[i].publish() {
                let mut recovery = Vec::new();
                for file in files[..=i].iter_mut().rev() {
                    if let Err(e) = file.rollback() {
                        recovery.push(format!("{e:#}"));
                    }
                }
                if recovery.is_empty() {
                    return Err(error);
                }
                bail!("{error:#}; activation file recovery needs attention: {}", recovery.join("; "));
            }
        }
        Ok(())
    }

    /// Clones (or adopts) the repo, authenticating with the new token.
    pub(crate) fn set_up_repo(&self) -> anyhow::Result<Joined> {
        self.set_up_repo_at(&self.state.repo)
    }

    fn set_up_repo_at(&self, repo: &Path) -> anyhow::Result<Joined> {
        let env = crate::auth::git_credentials_env(&self.state);
        let setup = plumbing::ensure_clone(repo, &self.state.repo_url(), &env).with_context(|| {
            format!("joined, but setting up {} failed; the daemon retries at startup", repo.display())
        })?;
        // An existing clone must continue the server's history (its branch was
        // checked before the code was spent; its history needs the server).
        if setup == Setup::Adopted {
            plumbing::check_shares_history(repo, &env)
                .with_context(|| format!("joined, but the existing clone at {} can't be used", repo.display()))?;
        }
        Ok(Joined {
            machine_id: self.state.machine_id.clone(),
            repo: self.state.repo.clone(),
            setup,
            harnesses: self.harnesses.clone(),
            revision: self.snapshot.revision,
        })
    }
}

/// A prepared repo, with both the old engine lock and the replacement lock
/// retained until credentials commit or the old directory is restored.
pub(crate) struct StandaloneRepo {
    pub(crate) joined: Joined,
    replacement: Option<Replacement>,
}
struct Replacement {
    repo: PathBuf,
    area: PathBuf,
    lock: Option<sk1ll_core::EngineLock>,
    had_original: bool,
    moved_original: bool,
    published: bool,
}
impl StandaloneRepo {
    /// Whether publishing moves a private clone into place (else the
    /// existing clone is used where it is).
    pub(crate) fn replaces(&self) -> bool {
        self.replacement.is_some()
    }

    pub(crate) fn publish(&mut self) -> anyhow::Result<()> {
        let Some(r) = &mut self.replacement else { return Ok(()) };
        if r.had_original {
            // Refuse edits made while the private clone was being built.
            let empty = std::fs::read_dir(&r.repo)?.next().is_none();
            if !empty && !plumbing::is_pristine_init(&r.repo)? {
                bail!("the standalone repo changed during join; move the draft aside and retry");
            }
            sk1ll_core::placement::rename_noreplace(&r.repo, &r.area.join("repo"))?;
            r.moved_original = true;
        }
        sk1ll_core::placement::rename_noreplace(&r.area.join("replacement"), &r.repo)?;
        r.published = true;
        Ok(())
    }
    pub(crate) fn rollback(&mut self) -> anyhow::Result<()> {
        let Some(r) = &mut self.replacement else { return Ok(()) };
        if r.published {
            // Preserve the failed clone, including any edits made while live.
            sk1ll_core::placement::rename_noreplace(&r.repo, &r.area.join("unactivated-repo"))
                .with_context(|| format!("preserving the failed clone in {}", r.area.display()))?;
            r.published = false;
        }
        if r.moved_original {
            sk1ll_core::placement::rename_noreplace(&r.area.join("repo"), &r.repo).with_context(|| {
                format!("restore {} from {}; its destination is never replaced", r.repo.display(), r.area.display())
            })?;
            r.moved_original = false;
        }
        Ok(())
    }
    pub(crate) fn take_live_lock(&mut self) -> Option<sk1ll_core::EngineLock> {
        self.replacement.as_mut().filter(|r| r.published).and_then(|r| r.lock.take())
    }
}

/// Every file is prepared in a private sibling directory. Old files and any
/// failed publication are retained there; rollback never clobbers a new leaf.
struct ActivationFile {
    target: PathBuf,
    area: PathBuf,
    backed_up: bool,
    published: bool,
    replace: bool,
}
impl ActivationFile {
    fn stage(target: PathBuf, value: &impl Serialize, replace: bool) -> anyhow::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        match target.symlink_metadata() {
            Ok(meta) if replace && meta.is_file() => {}
            Ok(_) => bail!("refusing activation over {}", target.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let parent = target.parent().context("activation path has no parent")?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let area = tempfile::Builder::new().prefix("daemon-activation-").tempdir_in(parent)?.keep();
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(area.join("staged"))?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(Self { target, area, backed_up: false, published: false, replace })
    }
    fn publish(&mut self) -> anyhow::Result<()> {
        if self.target.symlink_metadata().is_ok() {
            if !self.replace {
                bail!("refusing activation over {}", self.target.display());
            }
            sk1ll_core::placement::rename_noreplace(&self.target, &self.area.join("original"))?;
            self.backed_up = true;
        }
        sk1ll_core::placement::rename_noreplace(&self.area.join("staged"), &self.target)?;
        self.published = true;
        Ok(())
    }
    fn rollback(&mut self) -> anyhow::Result<()> {
        if self.published {
            sk1ll_core::placement::rename_noreplace(&self.target, &self.area.join("unactivated"))?;
            self.published = false;
        }
        if self.backed_up {
            sk1ll_core::placement::rename_noreplace(&self.area.join("original"), &self.target).with_context(|| {
                format!(
                    "restore {} from {} without replacing its destination",
                    self.target.display(),
                    self.area.display()
                )
            })?;
            self.backed_up = false;
        }
        Ok(())
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
