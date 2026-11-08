use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use sk1ll_core::{Assignment, MachineConfig};

/// Daemon settings: where its files live and how often it does things.
#[derive(Debug, Clone)]
pub struct Config {
    /// Machine id, server URL, token and repo path (`SK1LL_DAEMON_STATE`,
    /// default `~/.config/sk1ll/daemon.json`). Mode 0600.
    pub state: PathBuf,
    /// Harness config (`SK1LL_MACHINE_CONFIG`, default `~/.config/sk1ll/machine.toml`).
    pub machine_config: PathBuf,
    /// Assignment cache (`SK1LL_ASSIGNMENT`, default `~/.cache/sk1ll/assignment.json`).
    /// Only a mirror for the CLI's read-only commands: the daemon's own copy
    /// is the snapshot next to the state file ([`Config::snapshot_path`]).
    pub assignment: PathBuf,
    /// Local API socket (`SK1LL_DAEMON_SOCKET`, default
    /// `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`, else `run/daemon.sock` in the
    /// config dir). Its directory must be ours and mode 0700.
    pub socket: PathBuf,
    /// Fallback pull while connected (`SK1LL_PULL_INTERVAL`, default `5m`).
    pub pull_every: Duration,
    /// Quiet time before reacting to file changes (`SK1LL_DEBOUNCE`, default `300ms`).
    pub debounce: Duration,
    /// Longest a stream of file changes can postpone a sync
    /// (`SK1LL_DEBOUNCE_MAX`, default `2s`, at least `debounce`).
    pub debounce_max: Duration,
    /// Local re-sync of the harnesses, whether or not the server is reachable
    /// (`SK1LL_RECONCILE_INTERVAL`, default `60s`).
    pub reconcile_every: Duration,
    /// After the server has rejected the token this long, `/status` says the
    /// machine was probably removed (`SK1LL_AUTH_WARN_AFTER`, default `24h`).
    /// It is never revoked automatically.
    pub auth_warn_after: Duration,
    /// First reconnect delay; doubles up to `reconnect_max` (jittered).
    pub reconnect_min: Duration,
    /// `SK1LL_RECONNECT_MAX`, default `60s`.
    pub reconnect_max: Duration,
}

impl Config {
    /// Defaults from the platform's config, cache and runtime dirs.
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            state: config_dir()?.join("daemon.json"),
            machine_config: MachineConfig::default_path()?,
            assignment: Assignment::default_path()?,
            socket: Self::default_socket()?,
            pull_every: Duration::from_secs(300),
            debounce: Duration::from_millis(300),
            debounce_max: Duration::from_secs(2),
            reconcile_every: Duration::from_secs(60),
            auth_warn_after: Duration::from_secs(24 * 3600),
            reconnect_min: Duration::from_millis(500),
            reconnect_max: Duration::from_secs(60),
        })
    }

    /// [`Config::new`] adjusted by `SK1LL_*` environment variables.
    pub fn from_env() -> anyhow::Result<Self> {
        let mut cfg = Self::new()?;
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let Some(v) = var("SK1LL_DAEMON_STATE") {
            cfg.state = v.into();
        }
        if let Some(v) = var("SK1LL_MACHINE_CONFIG") {
            cfg.machine_config = v.into();
        }
        if let Some(v) = var("SK1LL_ASSIGNMENT") {
            cfg.assignment = v.into();
        }
        if let Some(v) = var("SK1LL_DAEMON_SOCKET") {
            cfg.socket = v.into();
        }
        let dur = |k: &str, d: &mut Duration| -> anyhow::Result<()> {
            if let Some(v) = var(k) {
                *d = parse_duration(&v).with_context(|| format!("{k}: `{v}`"))?;
            }
            Ok(())
        };
        dur("SK1LL_PULL_INTERVAL", &mut cfg.pull_every)?;
        dur("SK1LL_DEBOUNCE", &mut cfg.debounce)?;
        dur("SK1LL_DEBOUNCE_MAX", &mut cfg.debounce_max)?;
        dur("SK1LL_RECONCILE_INTERVAL", &mut cfg.reconcile_every)?;
        dur("SK1LL_AUTH_WARN_AFTER", &mut cfg.auth_warn_after)?;
        dur("SK1LL_RECONNECT_MAX", &mut cfg.reconnect_max)?;
        cfg.reconnect_min = cfg.reconnect_min.min(cfg.reconnect_max);
        cfg.debounce_max = cfg.debounce_max.max(cfg.debounce);
        Ok(cfg)
    }

    /// The assignment snapshot the daemon applies (revision and content in
    /// one file), next to the state file.
    pub fn snapshot_path(&self) -> PathBuf {
        self.state.with_file_name("daemon-assignment.json")
    }

    /// Undelivered `draft_discarded` messages, next to the state file.
    pub fn outbox_path(&self) -> PathBuf {
        self.state.with_file_name("daemon-outbox.json")
    }

    /// `SK1LL_DAEMON_SOCKET`, else `$XDG_RUNTIME_DIR/sk1ll/daemon.sock`, else
    /// `run/daemon.sock` in the config dir (a directory of its own, since the
    /// socket's directory must be private).
    pub fn default_socket() -> anyhow::Result<PathBuf> {
        if let Some(v) = std::env::var_os("SK1LL_DAEMON_SOCKET").filter(|v| !v.is_empty()) {
            return Ok(v.into());
        }
        match dirs::runtime_dir() {
            Some(d) => Ok(d.join("sk1ll").join("daemon.sock")),
            None => Ok(config_dir()?.join("run").join("daemon.sock")),
        }
    }
}

fn config_dir() -> anyhow::Result<PathBuf> {
    dirs::config_dir().map(|d| d.join("sk1ll")).context("cannot determine the config directory")
}

/// `500ms`, `90`, `90s`, `2m`, `1h`; a bare number is seconds.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().context("expected a number with an optional ms/s/m/h unit")?;
    let d = match unit.trim() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        u => bail!("unknown unit `{u}`; use ms, s, m or h"),
    };
    if d.is_zero() {
        bail!("must be greater than zero");
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("3w").is_err());
    }
}
